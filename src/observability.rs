//! Journal-derived low-cardinality efficiency metrics.
//!
//! The exporter is intentionally independent of Prometheus client state:
//! replaying the SQLite journal produces the same aggregate exposition after
//! restart, and no incident identity is emitted as a label.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::reasoning::journal::{IncidentJournal, JournalEvent};

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
    use super::render_journal_metrics;
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
            })
            .expect("append incident");
        let metrics = render_journal_metrics(store.journal());
        assert!(metrics.contains("ai_sre_incidents_opened_total 1"));
        assert!(!metrics.contains("secret-incident-id"));
        assert!(!metrics.contains("incident_id="));
        let _ = std::fs::remove_file(&path);
    }
}
