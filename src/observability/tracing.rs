//! Bounded, redacted OTLP/HTTP trace export for Tempo.
//!
//! Trace export is deliberately an observation side effect. `try_record` never
//! waits for the network or queue capacity, and the journal remains the source
//! of workflow truth when Tempo is unavailable.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use reqwest::Client;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::{self, error::TrySendError};

/// Resolves standard OTLP endpoint precedence; trace-specific URLs are used as-is.
/// Invalid endpoints disable export without affecting incident processing.
pub fn resolve_endpoint(base: Option<&str>, traces: Option<&str>) -> Option<String> {
    let selected = traces.or(base)?;
    let mut url = reqwest::Url::parse(selected).ok()?;
    if !["http", "https"].contains(&url.scheme())
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    if traces.is_none() {
        url.set_path(&format!("{}/v1/traces", url.path().trim_end_matches('/')));
    }
    Some(url.into())
}

/// Returns the stable Tempo lookup ID for an incident without disclosing its raw ID.
pub fn incident_trace_id(incident_id: &str) -> String {
    correlation_id(incident_id, 16)
}

/// Metadata-only trace event accepted by the exporter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TraceEvent {
    /// Stable trace correlation identifier.
    trace_id: String,
    /// Stable span identifier.
    span_id: String,
    /// Bounded operation name.
    name: String,
    /// Controlled workflow phase.
    phase: String,
    /// Optional provider/model alias, never a credential.
    provider: Option<String>,
    /// Event duration in milliseconds.
    duration_ms: u64,
    end_time_unix_nano: u64,
    outcome: &'static str,
}

impl TraceEvent {
    /// Builds an event while rejecting uncontrolled phases and payloads.
    pub fn new(
        trace_id: &str,
        span_id: &str,
        name: &str,
        phase: &str,
        provider: Option<&str>,
        duration_ms: u64,
    ) -> Option<Self> {
        if !super::ALLOWED_PHASES.contains(&phase)
            || !super::ALLOWED_TRACE_NAMES.contains(&name)
            || trace_id.trim().is_empty()
            || span_id.trim().is_empty()
            || trace_id.len() > 256
            || span_id.len() > 256
            || provider.is_some_and(|value| !["openai", "gemini", "deepseek"].contains(&value))
        {
            return None;
        }
        Some(Self {
            trace_id: incident_trace_id(trace_id),
            span_id: correlation_id(&format!("{span_id}:{name}"), 8),
            name: name.chars().take(128).collect(),
            phase: phase.to_owned(),
            provider: provider.map(|value| value.chars().take(64).collect()),
            duration_ms,
            outcome: "unspecified",
            end_time_unix_nano: u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_nanos(),
            )
            .ok()?,
        })
    }

    /// Attaches the terminal domain status without accepting model-generated labels.
    pub fn with_status(mut self, status: crate::reasoning::coordinator::RunStatus) -> Self {
        use crate::reasoning::{coordinator::RunStatus, router::ProviderKind};
        let (provider, outcome) = match status {
            RunStatus::Succeeded(ProviderKind::OpenAi) => ("openai", "succeeded"),
            RunStatus::Succeeded(ProviderKind::Gemini) => ("gemini", "succeeded"),
            RunStatus::Succeeded(ProviderKind::DeepSeek) => ("deepseek", "succeeded"),
            RunStatus::Succeeded(ProviderKind::Deterministic) => ("deterministic", "baseline"),
            RunStatus::Exhausted => ("deterministic", "exhausted"),
        };
        self.provider = Some(provider.into());
        self.outcome = outcome;
        self
    }

    /// Marks an application failure without serializing its potentially sensitive error.
    pub fn with_failure(mut self) -> Self {
        self.outcome = "failed";
        self.provider = None;
        self
    }
}

fn correlation_id(value: &str, bytes: usize) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .take(bytes)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Non-blocking exporter state.
#[derive(Clone)]
pub struct TraceExporter {
    sender: mpsc::Sender<TraceEvent>,
    dropped: Arc<AtomicU64>,
}

impl TraceExporter {
    /// Starts a bounded exporter worker. `None` disables export without a worker.
    pub fn start(endpoint: Option<String>, capacity: usize) -> Option<Self> {
        Self::configured(
            endpoint,
            crate::config::TracingConfig {
                queue_capacity: capacity,
                ..Default::default()
            },
        )
    }

    /// Starts export only for a valid full URL and bounded configuration.
    pub fn configured(
        endpoint: Option<String>,
        config: crate::config::TracingConfig,
    ) -> Option<Self> {
        if !config.is_valid() {
            return None;
        }
        let endpoint = resolve_endpoint(None, endpoint.as_deref())?;
        let (sender, mut receiver) = mpsc::channel::<TraceEvent>(config.queue_capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_worker = Arc::clone(&dropped);
        tokio::spawn(async move {
            let client = Client::builder()
                .timeout(std::time::Duration::from_millis(config.timeout_ms))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok();
            while let Some(event) = receiver.recv().await {
                let url = endpoint.as_str();
                let Some(client) = client.as_ref() else {
                    dropped_worker.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                let payload = serde_json::json!({
                    "resourceSpans": [{"resource":{"attributes":[
                        {"key":"service.name","value":{"stringValue":"ai-sre"}}
                    ]},"scopeSpans": [{"spans": [{
                        "traceId": event.trace_id, "spanId": event.span_id,
                        "startTimeUnixNano": event.end_time_unix_nano.saturating_sub(event.duration_ms.saturating_mul(1_000_000)).to_string(),
                        "endTimeUnixNano": event.end_time_unix_nano.to_string(),
                        "name": event.name, "attributes": [
                            {"key":"phase","value":{"stringValue":event.phase}},
                            {"key":"outcome","value":{"stringValue":event.outcome}},
                            {"key":"provider","value":{"stringValue":event.provider.unwrap_or_else(|| "none".into())}},
                            {"key":"duration_ms","value":{"intValue":event.duration_ms.to_string()}}
                        ]
                    }]}]}]
                });
                if !export(client, url, &payload, config.max_response_bytes).await {
                    dropped_worker.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        Some(Self { sender, dropped })
    }

    /// Enqueues an event without waiting for network or queue capacity.
    pub fn try_record(&self, event: TraceEvent) -> bool {
        match self.sender.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Closed(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Number of events dropped by queue or exporter failure.
    pub fn dropped_total(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

// A successful HTTP status can still reject spans. Bound the response before
// decoding it so an observation sink cannot allocate an unbounded body.
async fn export(
    client: &Client,
    url: &str,
    payload: &serde_json::Value,
    max_response_bytes: usize,
) -> bool {
    let Ok(mut response) = client.post(url).json(payload).send().await else {
        return false;
    };
    if response.status() != reqwest::StatusCode::OK {
        return false;
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= max_response_bytes => {
                body.extend_from_slice(&chunk)
            }
            Ok(None) => break,
            _ => return false,
        }
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return false;
    };
    if !value.is_object() {
        return false;
    }
    match value.get("partialSuccess") {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Object(fields)) => match fields.get("rejectedSpans") {
            None => true,
            Some(value) => value.as_u64() == Some(0) || value.as_str() == Some("0"),
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{TraceEvent, TraceExporter};

    #[tokio::test]
    async fn bounded_export_drops_without_waiting_for_tempo() {
        // Given an exporter whose endpoint is unavailable and a one-item queue.
        let exporter =
            TraceExporter::start(Some("http://127.0.0.1:1/v1/traces".into()), 1).expect("exporter");
        let event = || {
            TraceEvent::new(
                "trace",
                "span",
                "incident.investigation",
                "investigation",
                None,
                1,
            )
            .expect("event")
        };
        // When multiple events arrive faster than the exporter can deliver.
        assert!(exporter.try_record(event()));
        let _ = exporter.try_record(event());
        // Then submission remains bounded and exposes loss as an aggregate.
        assert!(exporter.dropped_total() <= 1);
    }

    #[test]
    fn trace_events_reject_sensitive_or_uncontrolled_phase_values() {
        // Given a raw log body and an unapproved phase.
        // When a trace event crosses the metadata boundary.
        // Then no sensitive body or uncontrolled phase can be exported.
        assert!(
            TraceEvent::new("trace", "span", "incident.investigation", "query", None, 1,).is_none()
        );
        assert!(
            TraceEvent::new(
                "trace",
                "span",
                "incident.investigation",
                "investigation",
                None,
                1,
            )
            .is_some()
        );
    }
}
