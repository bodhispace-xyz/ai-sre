//! Bounded, authenticated Alertmanager HTTP intake.
//!
//! Axum/Hyper owns HTTP framing and body limits. This module authenticates
//! before JSON parsing, normalizes only the documented webhook route, and
//! waits for the journal-owning worker to confirm durable admission.

use std::fmt;

use axum::{
    Router,
    body::Bytes,
    extract::{State, rejection::BytesRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use thiserror::Error;
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
};

use crate::reasoning::incident::{AlertmanagerWebhook, IncidentSignal, normalize_webhook};

/// The only HTTP route exposed by this intake.
pub const ALERTMANAGER_PATH: &str = "/webhooks/alertmanager";

/// Bounded request settings for the intake listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeConfig {
    /// Maximum request body accepted in bytes.
    pub max_body_bytes: usize,
}

impl Default for IntakeConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: 262_144,
        }
    }
}

/// Parsed webhook result returned by the transport boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeBatch {
    /// Normalized incidents from the webhook.
    pub incidents: Vec<IncidentSignal>,
}

/// A queue command whose acknowledgement is sent only after durable commit.
#[derive(Debug)]
pub struct IntakeCommand {
    /// Normalized incidents to append and correlate.
    pub batch: IntakeBatch,
    /// Worker response for durable admission.
    pub acknowledged: oneshot::Sender<Result<(), ()>>,
}

/// Safe HTTP intake failures used by the parser and worker boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum IntakeError {
    /// The method or route is outside the webhook contract.
    #[error("HTTP route is not allowed")]
    NotAllowed,
    /// The body exceeded the configured limit.
    #[error("HTTP request body is too large")]
    BodyTooLarge,
    /// The HTTP framing or JSON payload is malformed.
    #[error("HTTP webhook payload is malformed")]
    Malformed,
    /// Authentication was missing or invalid.
    #[error("HTTP webhook authentication failed")]
    Unauthorized,
    /// The listener or connection failed.
    #[error("HTTP intake I/O failed")]
    Io,
    /// No worker was available to accept the batch.
    #[error("HTTP intake queue is unavailable")]
    QueueUnavailable,
}

#[derive(Clone)]
struct IntakeState {
    intake: AlertIntake,
    sender: mpsc::Sender<IntakeCommand>,
}

/// Bounded Alertmanager webhook listener.
#[derive(Clone)]
pub struct AlertIntake {
    config: IntakeConfig,
    current_token: Option<SecretToken>,
    next_token: Option<SecretToken>,
}

impl fmt::Debug for AlertIntake {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AlertIntake")
            .field("config", &self.config)
            .field(
                "current_token",
                &self.current_token.as_ref().map(|_| "[redacted]"),
            )
            .field(
                "next_token",
                &self.next_token.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

#[derive(Clone)]
struct SecretToken(String);

impl fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

impl AlertIntake {
    /// Creates an intake with explicit request bounds and no authentication.
    pub fn new(config: IntakeConfig) -> Self {
        Self {
            config,
            current_token: None,
            next_token: None,
        }
    }

    /// Requires the current bearer token and optionally accepts a rotation key.
    pub fn with_bearer_tokens(
        mut self,
        current: impl Into<String>,
        next: Option<impl Into<String>>,
    ) -> Self {
        self.current_token = Some(SecretToken(current.into()));
        self.next_token = next.map(|token| SecretToken(token.into()));
        self
    }

    /// Parses one complete request for focused parser tests.
    pub fn parse_request(&self, request: &[u8]) -> Result<IntakeBatch, IntakeError> {
        let separator = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .ok_or(IntakeError::Malformed)?;
        let (head, body) = request.split_at(separator + 4);
        let head = std::str::from_utf8(head).map_err(|_| IntakeError::Malformed)?;
        let mut lines = head.split("\r\n");
        let request_line = lines.next().ok_or(IntakeError::Malformed)?;
        let mut request_parts = request_line.split_whitespace();
        if request_parts.next() != Some("POST") || request_parts.next() != Some(ALERTMANAGER_PATH) {
            return Err(IntakeError::NotAllowed);
        }
        let content_length = lines
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .ok_or(IntakeError::Malformed)?;
        if content_length > self.config.max_body_bytes || body.len() != content_length {
            return Err(IntakeError::BodyTooLarge);
        }
        parse_body(body)
    }

    /// Serves the webhook forever with bounded framing and durable admission.
    pub async fn serve(
        self,
        listener: TcpListener,
        sender: mpsc::Sender<IntakeCommand>,
    ) -> Result<(), IntakeError> {
        let max_body_bytes = self.config.max_body_bytes;
        let state = IntakeState {
            intake: self,
            sender,
        };
        let app = Router::new()
            .route(ALERTMANAGER_PATH, post(handle_webhook))
            .with_state(state)
            .layer(axum::extract::DefaultBodyLimit::max(max_body_bytes));
        axum::serve(listener, app)
            .await
            .map_err(|_| IntakeError::Io)
    }
}

async fn handle_webhook(
    State(state): State<IntakeState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let result = async {
        authenticate(&state.intake, &headers)?;
        let body = body.map_err(|_| IntakeError::BodyTooLarge)?;
        if body.len() > state.intake.config.max_body_bytes {
            return Err(IntakeError::BodyTooLarge);
        }
        let batch = parse_body(&body)?;
        let (acknowledged, response) = oneshot::channel();
        state
            .sender
            .send(IntakeCommand {
                batch,
                acknowledged,
            })
            .await
            .map_err(|_| IntakeError::QueueUnavailable)?;
        response
            .await
            .map_err(|_| IntakeError::QueueUnavailable)?
            .map_err(|_| IntakeError::QueueUnavailable)?;
        Ok::<_, IntakeError>(())
    }
    .await;

    match result {
        Ok(()) => response_with_close(StatusCode::ACCEPTED),
        Err(IntakeError::Unauthorized) => response_with_close(StatusCode::UNAUTHORIZED),
        Err(IntakeError::BodyTooLarge) => response_with_close(StatusCode::PAYLOAD_TOO_LARGE),
        Err(IntakeError::NotAllowed) => response_with_close(StatusCode::NOT_FOUND),
        Err(IntakeError::QueueUnavailable | IntakeError::Io) => {
            response_with_close(StatusCode::SERVICE_UNAVAILABLE)
        }
        Err(IntakeError::Malformed) => response_with_close(StatusCode::BAD_REQUEST),
    }
}

fn response_with_close(status: StatusCode) -> Response {
    let mut response = status.into_response();
    response.headers_mut().insert(
        axum::http::header::CONNECTION,
        axum::http::HeaderValue::from_static("close"),
    );
    response
}

fn parse_body(body: &[u8]) -> Result<IntakeBatch, IntakeError> {
    let webhook: AlertmanagerWebhook =
        serde_json::from_slice(body).map_err(|_| IntakeError::Malformed)?;
    Ok(IntakeBatch {
        incidents: normalize_webhook(webhook),
    })
}

fn authenticate(intake: &AlertIntake, headers: &HeaderMap) -> Result<(), IntakeError> {
    let Some(current) = &intake.current_token else {
        return Ok(());
    };
    let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return Err(IntakeError::Unauthorized);
    };
    let Some(candidate) = value.strip_prefix("Bearer ") else {
        return Err(IntakeError::Unauthorized);
    };
    let valid_current = constant_time_equal(candidate.as_bytes(), current.0.as_bytes());
    let valid_next = intake
        .next_token
        .as_ref()
        .is_some_and(|next| constant_time_equal(candidate.as_bytes(), next.0.as_bytes()));
    if valid_current || valid_next {
        Ok(())
    } else {
        Err(IntakeError::Unauthorized)
    }
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut difference = left.len() ^ right.len();
    for index in 0..max_len {
        difference |= usize::from(left.get(index).copied().unwrap_or_default())
            ^ usize::from(right.get(index).copied().unwrap_or_default());
    }
    difference == 0
}
