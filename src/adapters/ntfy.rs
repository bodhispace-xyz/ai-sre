//! Read-only notification boundary for shadow investigation reports.
//!
//! ntfy is treated as a delivery and interaction surface only. A published
//! message contains a recommendation, never an authorization token or an
//! executable command.

use reqwest::{StatusCode, Url};
use std::time::Duration;
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
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("notification HTTP client configuration must be valid"),
            config,
            access_token,
        }
    }

    /// Publishes one shadow recommendation; no action is authorized here.
    pub async fn publish(&self, result: &InvestigationResult) -> Result<(), NtfyError> {
        self.publish_message(&render_message(result)).await
    }

    /// Publishes an already-rendered outbox message with the same bounded
    /// transport and authentication policy.
    pub async fn publish_message(&self, message: &str) -> Result<(), NtfyError> {
        self.publish_internal(None, message).await
    }

    /// Publishes an outbox message with its stable delivery identity.
    pub async fn publish_with_delivery_id(
        &self,
        delivery_id: &str,
        message: &str,
    ) -> Result<(), NtfyError> {
        self.publish_internal(Some(delivery_id), message).await
    }

    async fn publish_internal(
        &self,
        delivery_id: Option<&str>,
        message: &str,
    ) -> Result<(), NtfyError> {
        if !self.config.is_valid() || message.len() > 64 * 1024 {
            return Err(NtfyError::Rejected);
        }
        let url = format!(
            "{}/{}",
            self.config.endpoint.trim_end_matches('/'),
            self.config.topic
        );
        let mut request = self
            .client
            .post(url)
            .header("Title", "AI SRE shadow investigation")
            .header("Tags", "mag,robot_face")
            .body(message.to_owned());
        if let Some(token) = &self.access_token {
            request = request.bearer_auth(token);
        }
        if let Some(delivery_id) = delivery_id {
            request = request.header("X-Idempotency-Key", delivery_id);
        }
        let response = request.send().await.map_err(|_| NtfyError::Transport)?;
        if response.status() != StatusCode::OK {
            return Err(NtfyError::Rejected);
        }
        Ok(())
    }
}

impl NtfyConfig {
    /// Validates the server-owned endpoint and single publish topic.
    pub fn is_valid(&self) -> bool {
        let Ok(url) = Url::parse(self.endpoint.trim()) else {
            return false;
        };
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.host_str().is_some()
            && !self.topic.trim().is_empty()
            && self.topic.len() <= 128
            && !self.topic.contains('/')
            && !self.topic.contains(['?', '#'])
            && !self.topic.chars().any(char::is_control)
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
    render_message_with_view(result, None)
}

/// Renders a report with an optional server-owned read-only incident link.
pub fn render_message_with_view(
    result: &InvestigationResult,
    view_base_url: Option<&str>,
) -> String {
    let provider = crate::reasoning::investigation::ShadowInvestigator::terminal_provider(result)
        .map_or_else(
            || "deterministic-baseline".to_owned(),
            |provider| format!("{provider:?}"),
        );
    let view = view_base_url
        .and_then(|base| {
            let mut url = Url::parse(base.trim()).ok()?;
            url.path_segments_mut().ok()?.push(&result.incident_id);
            Some(format!("\nView: {url}"))
        })
        .unwrap_or_default();
    let summary = crate::reasoning::investigation::redact_text(&result.report.summary)
        .chars()
        .take(60_000)
        .collect::<String>();
    format!(
        "Incident: {}\nProvider: {provider}\nSummary: {}\nEvidence: {} item(s)\nMode: shadow; no action executed.{view}",
        result.incident_id,
        summary,
        result.evidence_ids.len()
    )
}

#[cfg(test)]
mod tests {
    use super::NtfyConfig;

    #[test]
    fn ntfy_config_allows_one_publish_topic_only() {
        // Given a server-owned HTTPS endpoint and topic.
        let valid = NtfyConfig {
            endpoint: "https://ntfy.example".to_owned(),
            topic: "ai-sre".to_owned(),
        };
        let invalid = NtfyConfig {
            endpoint: "http://ntfy.example".to_owned(),
            topic: "other/topic".to_owned(),
        };

        // When notification configuration crosses the publisher boundary.
        // Then only the bounded publish destination is accepted.
        assert!(valid.is_valid());
        assert!(!invalid.is_valid());
    }

    #[test]
    fn ntfy_config_rejects_url_userinfo_and_query_delimiters() {
        // Given destinations that could redirect credentials or alter the topic path.
        let userinfo = NtfyConfig {
            endpoint: "https://expected.example@attacker.example".to_owned(),
            topic: "ai-sre".to_owned(),
        };
        let query_topic = NtfyConfig {
            endpoint: "https://ntfy.example".to_owned(),
            topic: "ai-sre?x=1".to_owned(),
        };

        // When server-owned notification configuration is validated.
        // Then deceptive authorities and URL delimiters are rejected.
        assert!(!userinfo.is_valid());
        assert!(!query_topic.is_valid());
    }
}
