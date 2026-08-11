//! Read-only notification boundary for shadow investigation reports.
//!
//! ntfy is treated as a delivery and interaction surface only. A published
//! message contains a recommendation, never an authorization token or an
//! executable command.

use reqwest::StatusCode;
use thiserror::Error;

use crate::reasoning::investigation::InvestigationResult;

/// Non-secret ntfy destination settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtfyConfig {
    /// Base ntfy endpoint, for example `https://ntfy.example`.
    pub endpoint: String,
    /// Topic receiving shadow reports.
    pub topic: String,
}

/// A publisher for redacted shadow reports.
pub struct NtfyPublisher {
    client: reqwest::Client,
    config: NtfyConfig,
    access_token: Option<String>,
}

impl NtfyPublisher {
    /// Creates a publisher with an optional private bearer token.
    pub fn new(config: NtfyConfig, access_token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
            access_token,
        }
    }

    /// Publishes one shadow recommendation; no action is authorized here.
    pub async fn publish(&self, result: &InvestigationResult) -> Result<(), NtfyError> {
        let url = format!(
            "{}/{}",
            self.config.endpoint.trim_end_matches('/'),
            self.config.topic
        );
        let message = render_message(result);
        let mut request = self
            .client
            .post(url)
            .header("Title", "AI SRE shadow investigation")
            .header("Tags", "mag,robot_face")
            .body(message);
        if let Some(token) = &self.access_token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|_| NtfyError::Transport)?;
        if response.status() != StatusCode::OK {
            return Err(NtfyError::Rejected);
        }
        Ok(())
    }
}

/// Safe failures from the notification transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum NtfyError {
    /// The request could not reach the configured endpoint.
    #[error("ntfy notification transport failed")]
    Transport,
    /// ntfy rejected the publication.
    #[error("ntfy notification was rejected")]
    Rejected,
}

/// Renders a bounded operator-facing report without raw tool payloads.
pub fn render_message(result: &InvestigationResult) -> String {
    let provider = crate::reasoning::investigation::ShadowInvestigator::terminal_provider(result)
        .map_or_else(
            || "deterministic-baseline".to_owned(),
            |provider| format!("{provider:?}"),
        );
    format!(
        "Incident: {}\nProvider: {provider}\nSummary: {}\nEvidence: {} item(s)\nMode: shadow; no action executed.",
        result.incident_id,
        result.report.summary,
        result.evidence_ids.len()
    )
}
