//! Minimal bounded HTTP transport for the Alertmanager intake.
//!
//! The transport accepts one narrow webhook route, bounds request size, and
//! delegates all incident identity decisions to the reasoning boundary.

use std::collections::HashMap;

use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::reasoning::incident::{AlertmanagerWebhook, IncidentSignal, normalize_webhook};

/// The only HTTP route exposed by this intake.
pub const ALERTMANAGER_PATH: &str = "/webhooks/alertmanager";

/// Bounded request settings for the intake listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Safe HTTP intake failures.
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
    /// The listener or connection failed.
    #[error("HTTP intake I/O failed")]
    Io,
    /// No worker was available to accept the batch.
    #[error("HTTP intake queue is unavailable")]
    QueueUnavailable,
}

/// Bounded Alertmanager webhook listener.
#[derive(Debug, Clone, Copy)]
pub struct AlertIntake {
    config: IntakeConfig,
}

impl AlertIntake {
    /// Creates an intake with explicit request bounds.
    pub const fn new(config: IntakeConfig) -> Self {
        Self { config }
    }

    /// Parses one complete HTTP request without executing any incident work.
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
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
            .collect::<HashMap<_, _>>();
        let content_length = headers
            .get("content-length")
            .and_then(|length| length.parse::<usize>().ok())
            .ok_or(IntakeError::Malformed)?;
        if content_length > self.config.max_body_bytes || body.len() != content_length {
            return Err(IntakeError::BodyTooLarge);
        }
        let webhook: AlertmanagerWebhook =
            serde_json::from_slice(body).map_err(|_| IntakeError::Malformed)?;
        Ok(IntakeBatch {
            incidents: normalize_webhook(webhook),
        })
    }

    /// Serves connections forever, acknowledging accepted batches only.
    pub async fn serve(
        self,
        listener: TcpListener,
        sender: mpsc::Sender<IntakeBatch>,
    ) -> Result<(), IntakeError> {
        loop {
            let (stream, _) = listener.accept().await.map_err(|_| IntakeError::Io)?;
            let intake = self;
            let sender = sender.clone();
            tokio::spawn(async move {
                let _ = intake.handle(stream, &sender).await;
            });
        }
    }

    async fn handle(
        &self,
        mut stream: TcpStream,
        sender: &mpsc::Sender<IntakeBatch>,
    ) -> Result<(), IntakeError> {
        let mut request = vec![0_u8; self.config.max_body_bytes.saturating_add(16_384)];
        let size = stream
            .read(&mut request)
            .await
            .map_err(|_| IntakeError::Io)?;
        request.truncate(size);
        let response = match self.parse_request(&request) {
            Ok(batch) => match sender.try_send(batch) {
                Ok(()) => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                Err(_) => {
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                }
            },
            Err(IntakeError::NotAllowed) => {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            }
            Err(IntakeError::BodyTooLarge) => {
                "HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            }
            Err(IntakeError::Malformed) => {
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            }
            Err(IntakeError::Io) => return Err(IntakeError::Io),
            Err(IntakeError::QueueUnavailable) => return Err(IntakeError::QueueUnavailable),
        };
        stream
            .write_all(response.as_bytes())
            .await
            .map_err(|_| IntakeError::Io)
    }
}
