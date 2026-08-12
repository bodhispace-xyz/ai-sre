//! Journal-derived low-cardinality efficiency metrics.
//!
//! The exporter is intentionally independent of Prometheus client state:
//! replaying the SQLite journal produces the same aggregate exposition after
//! restart, and no incident identity is emitted as a label.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::reasoning::journal::{IncidentJournal, JournalEvent};

/// Low-cardinality phase labels accepted by telemetry emitters.
pub const ALLOWED_PHASES: &[&str] = &["investigation", "reasoning", "human_wait", "execution"];

/// Bounded structured log fields. Payloads and query text are excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredLog {
    /// Stable lifecycle event name.
    pub event: String,
    /// Incident identity retained as a log field, never a metric label.
    pub incident_id: String,
    /// Run identity retained as a log field, never a metric label.
    pub run_id: String,
    /// Optional bounded phase name.
    pub phase: Option<String>,
}

impl StructuredLog {
    /// Creates a bounded structured event and rejects unknown phase labels.
    pub fn new(
        event: impl Into<String>,
        incident_id: impl Into<String>,
        run_id: impl Into<String>,
        phase: Option<&str>,
    ) -> Option<Self> {
        let phase = phase.map(str::to_owned);
        if phase
            .as_deref()
            .is_some_and(|value| !ALLOWED_PHASES.contains(&value))
        {
            return None;
        }
        Some(Self {
            event: event.into().chars().take(128).collect(),
            incident_id: incident_id.into().chars().take(128).collect(),
            run_id: run_id.into().chars().take(128).collect(),
            phase,
        })
    }
}

/// Bounded OTel-style span fields; export failure is non-blocking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanContext {
    /// Operation name.
    pub name: String,
    /// Controlled phase label.
    pub phase: String,
}

/// Creates a span context only for an allowlisted phase.
pub fn span_context(name: &str, phase: &str) -> Option<SpanContext> {
    ALLOWED_PHASES.contains(&phase).then(|| SpanContext {
        name: name.chars().take(128).collect(),
        phase: phase.to_owned(),
    })
}

/// In-process snapshot shared by the worker and the metrics HTTP handler.
#[derive(Clone, Default)]
pub struct MetricsSnapshot(Arc<RwLock<String>>);

impl MetricsSnapshot {
    /// Replaces the exposition snapshot after a durable journal update.
    pub async fn replace_from(&self, journal: &IncidentJournal) {
        *self.0.write().await = render_journal_metrics(journal);
    }

    /// Returns the latest exposition payload.
    pub async fn read(&self) -> String {
        self.0.read().await.clone()
    }
}

/// Renders fixed-name Prometheus aggregates from raw journal facts.
pub fn render_journal_metrics(journal: &IncidentJournal) -> String {
    let projection = journal.project();
    let mut incidents_opened = 0_u64;
    let mut incidents_recovered = 0_u64;
    let mut incidents_completed = 0_u64;
    let mut incidents_resumed = 0_u64;
    for entry in journal.entries() {
        match entry.event {
            JournalEvent::IncidentOpened { .. } => incidents_opened += 1,
            JournalEvent::IncidentRecovered { .. } => incidents_recovered += 1,
            JournalEvent::IncidentCompleted { .. } => incidents_completed += 1,
            JournalEvent::IncidentResumed { .. } => incidents_resumed += 1,
            JournalEvent::ToolContext { .. } => {}
            _ => {}
        }
    }

    let mut output = String::new();
    metric(
        &mut output,
        "ai_sre_incidents_opened_total",
        "Total normalized firing incidents.",
        incidents_opened,
    );
    metric(
        &mut output,
        "ai_sre_incidents_recovered_total",
        "Total normalized recovery events.",
        incidents_recovered,
    );
    metric(
        &mut output,
        "ai_sre_incidents_completed_total",
        "Total incidents with a terminal report.",
        incidents_completed,
    );
    metric(
        &mut output,
        "ai_sre_incidents_resumed_total",
        "Total incomplete incidents resumed after redelivery or restart.",
        incidents_resumed,
    );
    metric(
        &mut output,
        "ai_sre_active_machine_time_ms_total",
        "Aggregate active non-human phase time in milliseconds.",
        projection.active_machine_ms,
    );
    metric(
        &mut output,
        "ai_sre_provider_time_ms_total",
        "Aggregate provider adapter time in milliseconds.",
        projection.provider_time_ms,
    );
    metric(
        &mut output,
        "ai_sre_human_wait_time_ms_total",
        "Aggregate human approval wait time in milliseconds.",
        projection.human_wait_ms,
    );
    metric(
        &mut output,
        "ai_sre_provider_attempts_total",
        "Aggregate provider attempts.",
        u64::from(projection.provider_attempts),
    );
    metric(
        &mut output,
        "ai_sre_evidence_queries_total",
        "Aggregate evidence queries charged to provider attempts.",
        u64::from(projection.evidence_queries),
    );
    metric(
        &mut output,
        "ai_sre_tool_calls_total",
        "Aggregate model-directed read-only context calls.",
        u64::from(projection.tool_calls),
    );
    metric(
        &mut output,
        "ai_sre_successful_tool_calls_total",
        "Aggregate context calls that committed evidence.",
        u64::from(projection.successful_tool_calls),
    );
    metric(
        &mut output,
        "ai_sre_tool_time_ms_total",
        "Aggregate bounded context-call time in milliseconds.",
        projection.tool_elapsed_ms,
    );
    metric(
        &mut output,
        "ai_sre_tool_output_bytes_total",
        "Aggregate bounded context-result bytes retained for resumption.",
        projection.tool_output_bytes,
    );
    metric(
        &mut output,
        "ai_sre_tokens_total",
        "Aggregate trusted provider token usage.",
        projection.tokens,
    );
    metric(
        &mut output,
        "ai_sre_known_cost_micro_usd_total",
        "Aggregate known estimated provider cost in integer micro-USD.",
        projection.known_cost_micro_usd,
    );
    metric(
        &mut output,
        "ai_sre_unknown_cost_attempts_total",
        "Provider attempts whose monetary cost remains unknown.",
        u64::from(projection.unknown_cost_attempts),
    );
    output
}

fn metric(output: &mut String, name: &str, help: &str, value: u64) {
    output.push_str("# HELP ");
    output.push_str(name);
    output.push(' ');
    output.push_str(help);
    output.push('\n');
    output.push_str("# TYPE ");
    output.push_str(name);
    output.push_str(" counter\n");
    output.push_str(name);
    output.push(' ');
    output.push_str(&value.to_string());
    output.push('\n');
}

#[cfg(test)]
mod tests {
    use super::{StructuredLog, render_journal_metrics, span_context};
    use crate::reasoning::{journal::JournalEvent, storage::JournalStore};

    #[test]
    fn metrics_use_fixed_names_without_incident_labels() {
        let path =
            std::env::temp_dir().join(format!("ai-sre-metrics-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut store = JournalStore::open(&path).expect("open journal");
        store
            .append(JournalEvent::IncidentOpened {
                incident_id: "secret-incident-id".to_owned(),
                alert_name: "ApiDown".to_owned(),
                labels: std::collections::BTreeMap::new(),
                annotations: std::collections::BTreeMap::new(),
                event_time: String::new(),
                source_event_id: String::new(),
            })
            .expect("append incident");
        store
            .append(JournalEvent::ToolContext {
                provider: crate::reasoning::router::ProviderKind::OpenAi,
                call_id: "call-1".to_owned(),
                tool: "query_logs".to_owned(),
                query_digest: crate::reasoning::journal::query_digest("{app=\"api\"}"),
                result_class: crate::reasoning::tools::ToolResultClass::Succeeded,
                elapsed_ms: 12,
                output_bytes: 128,
                evidence_queries_before: 0,
                evidence_queries_after: 1,
                at_ms: 2,
            })
            .expect("append tool fact");
        let metrics = render_journal_metrics(store.journal());
        assert!(metrics.contains("ai_sre_incidents_opened_total 1"));
        assert!(metrics.contains("ai_sre_tool_calls_total 1"));
        assert!(metrics.contains("ai_sre_successful_tool_calls_total 1"));
        assert!(metrics.contains("ai_sre_tool_output_bytes_total 128"));
        assert!(!metrics.contains("secret-incident-id"));
        assert!(!metrics.contains("incident_id="));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn telemetry_rejects_uncontrolled_labels_and_keeps_ids_out_of_metrics() {
        // Given a structured lifecycle event and an attacker-controlled phase.
        let event = StructuredLog::new("provider.finish", "incident-1", "run-1", Some("reasoning"));
        let rejected = StructuredLog::new("provider.finish", "incident-1", "run-1", Some("query"));

        // When telemetry fields are constructed.
        // Then only controlled phases produce logs or spans.
        assert!(event.is_some());
        assert!(rejected.is_none());
        assert!(span_context("provider", "reasoning").is_some());
        assert!(span_context("provider", "query").is_none());
    }
}
