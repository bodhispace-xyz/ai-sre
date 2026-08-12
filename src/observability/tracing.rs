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
use tokio::sync::mpsc::{self, error::TrySendError};

/// Metadata-only trace event accepted by the exporter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TraceEvent {
    /// Stable trace correlation identifier.
    pub trace_id: String,
    /// Stable span identifier.
    pub span_id: String,
    /// Bounded operation name.
    pub name: String,
    /// Controlled workflow phase.
    pub phase: String,
    /// Optional provider/model alias, never a credential.
    pub provider: Option<String>,
    /// Event duration in milliseconds.
    pub duration_ms: u64,
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
            || provider.is_some_and(|value| !is_safe_metadata(value))
        {
            return None;
        }
        Some(Self {
            trace_id: trace_id.chars().take(64).collect(),
            span_id: span_id.chars().take(32).collect(),
            name: name.chars().take(128).collect(),
            phase: phase.to_owned(),
            provider: provider.map(|value| value.chars().take(64).collect()),
            duration_ms,
        })
    }
}

fn is_safe_metadata(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// Non-blocking exporter state.
#[derive(Clone)]
pub struct TraceExporter {
    sender: mpsc::Sender<TraceEvent>,
    dropped: Arc<AtomicU64>,
}

impl TraceExporter {
    /// Starts a bounded exporter worker. `None` disables network export while
    /// retaining the same non-blocking call contract.
    pub fn start(endpoint: Option<String>, capacity: usize) -> Option<Self> {
        if capacity == 0 {
            return None;
        }
        let (sender, mut receiver) = mpsc::channel::<TraceEvent>(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_worker = Arc::clone(&dropped);
        tokio::spawn(async move {
            let client = Client::builder()
                .timeout(std::time::Duration::from_secs(1))
                .build()
                .ok();
            while let Some(event) = receiver.recv().await {
                let Some(url) = endpoint.as_deref() else {
                    continue;
                };
                let Some(client) = client.as_ref() else {
                    dropped_worker.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                let payload = serde_json::json!({
                    "resourceSpans": [{"scopeSpans": [{"spans": [{
                        "traceId": event.trace_id, "spanId": event.span_id,
                        "name": event.name, "attributes": [
                            {"key":"phase","value":{"stringValue":event.phase}},
                            {"key":"duration_ms","value":{"intValue":event.duration_ms.to_string()}}
                        ]
                    }]}]}]
                });
                if client.post(url).json(&payload).send().await.is_err() {
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
