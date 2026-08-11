//! Shadow-mode investigation orchestration.
//!
//! This service sequences normalized incidents through bounded Grafana
//! evidence collection and the existing finite provider runtime. It records
//! lifecycle facts but never invokes the gateway or performs mutations.

use std::collections::BTreeSet;
use tokio::time::timeout;

use thiserror::Error;

use crate::adapters::grafana::context::{ContextBudget, ContextError, GrafanaContext};

use super::{
    budget::Reservation,
    incident::IncidentSignal,
    journal::{JournalContext, JournalEvent, Phase},
    live::{LiveProviders, run_live},
    recorded::RecordedProvider,
    router::ProviderKind,
    runtime::{IncidentRuntime, RuntimeError},
    storage::{JournalStore, JournalStoreError},
};

/// Explicit read-only queries selected for one shadow investigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvestigationQueries {
    /// LogQL expression for Loki.
    pub logs: String,
    /// PromQL expression for Prometheus.
    pub metrics: String,
}

/// Result of one bounded shadow investigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvestigationResult {
    /// Stable incident identity.
    pub incident_id: String,
    /// Evidence IDs available to the provider report.
    pub evidence_ids: BTreeSet<String>,
    /// Terminal provider state.
    pub status: super::coordinator::RunStatus,
    /// Accepted provider report, or a deterministic exhaustion explanation.
    pub report: super::contracts::DiagnosticReport,
}

/// Failures that stop a shadow investigation before mutation is possible.
#[derive(Debug, Error)]
pub enum InvestigationError {
    /// Grafana returned no usable bounded evidence.
    #[error("shadow investigation could not collect Grafana evidence")]
    Context(#[from] ContextError),
    /// The reasoning runtime rejected the workflow transition.
    #[error("shadow investigation reasoning failed")]
    Runtime(#[from] RuntimeError),
    /// The durable journal could not record a lifecycle fact.
    #[error("shadow investigation journal write failed")]
    Journal(#[from] JournalStoreError),
    /// The incident wall-clock budget expired before completion.
    #[error("shadow investigation wall-clock budget expired")]
    Deadline,
}

/// Inputs for one live-provider investigation.
pub struct LiveInvestigationInput<'a> {
    /// Read-only Grafana context.
    pub grafana: &'a GrafanaContext,
    /// Single-writer durable journal.
    pub journal: &'a mut JournalStore,
    /// Normalized incident signal.
    pub signal: &'a IncidentSignal,
    /// Stable run identity for this investigation attempt.
    pub run_id: &'a str,
    /// Fresh per-incident reasoning runtime.
    pub runtime: &'a mut IncidentRuntime,
    /// Configured live providers.
    pub providers: LiveProviders<'a>,
    /// Worst-case reservation for each provider restart.
    pub reservation: Reservation,
    /// Explicit read-only evidence queries.
    pub queries: InvestigationQueries,
    /// Monotonic incident timestamp.
    pub start_at_ms: u64,
}

/// Runs one live-provider investigation using an existing single-writer journal.
pub async fn investigate_live(
    input: LiveInvestigationInput<'_>,
) -> Result<InvestigationResult, InvestigationError> {
    let LiveInvestigationInput {
        grafana,
        journal,
        signal,
        run_id,
        runtime,
        providers,
        reservation,
        queries,
        start_at_ms,
    } = input;
    let context = JournalContext {
        incident_id: signal.incident_id.clone(),
        run_id: run_id.to_owned(),
    };
    journal.append_scoped(
        JournalEvent::PhaseStarted {
            phase: Phase::Investigation,
            at_ms: start_at_ms,
        },
        Some(&context),
    )?;
    let mut collected = super::evidence::EvidenceBoard::default();
    let mut context_budget = ContextBudget::new(2);
    let remaining = runtime.remaining().ok_or(InvestigationError::Deadline)?;
    timeout(
        remaining,
        grafana.logs_with_budget(&mut collected, &mut context_budget, queries.logs),
    )
    .await
    .map_err(|_| InvestigationError::Deadline)??;
    let remaining = runtime.remaining().ok_or(InvestigationError::Deadline)?;
    timeout(
        remaining,
        grafana.metrics_with_budget(&mut collected, &mut context_budget, queries.metrics),
    )
    .await
    .map_err(|_| InvestigationError::Deadline)??;
    for record in collected.records() {
        runtime.commit_evidence(
            record.source,
            record.query.clone(),
            record.payload.clone(),
            runtime.elapsed_ms(),
        )?;
    }
    journal.append_scoped(
        JournalEvent::PhaseFinished {
            phase: Phase::Investigation,
            at_ms: runtime.elapsed_ms(),
        },
        Some(&context),
    )?;
    let prompt = build_prompt(signal, runtime);
    let status = run_live(runtime, providers, &prompt, reservation, start_at_ms).await?;
    for entry in runtime.journal().entries() {
        journal.append_scoped(entry.event.clone(), Some(&context))?;
    }
    Ok(InvestigationResult {
        incident_id: signal.incident_id.clone(),
        evidence_ids: runtime
            .evidence()
            .records()
            .iter()
            .map(|record| record.evidence_id.clone())
            .collect(),
        status,
        report: runtime.last_report().cloned().unwrap_or_else(|| {
            super::contracts::DiagnosticReport {
                summary: "No provider produced an accepted diagnosis within the incident budget."
                    .to_owned(),
                evidence: Vec::new(),
            }
        }),
    })
}

fn build_prompt(signal: &IncidentSignal, runtime: &IncidentRuntime) -> String {
    let mut prompt = format!(
        "Diagnose incident {} (alert {}). Return strict JSON with summary and evidence citations.\n",
        signal.incident_id, signal.alert_name
    );
    for record in runtime.evidence().records() {
        let payload = redact_text(&String::from_utf8_lossy(&record.payload));
        prompt.push_str(&format!(
            "Evidence {} ({:?}, query={}): {}\n",
            record.evidence_id, record.source, record.query, payload
        ));
        if prompt.len() >= 32_768 {
            let mut limit = 32_768;
            while !prompt.is_char_boundary(limit) {
                limit -= 1;
            }
            prompt.truncate(limit);
            break;
        }
    }
    prompt
}

/// Redacts common credential-bearing fields before evidence crosses an LLM or
/// operator-notification boundary.
pub fn redact_text(input: &str) -> String {
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(input) {
        redact_json(&mut value);
        return serde_json::to_string(&value).unwrap_or_else(|_| "[REDACTED EVIDENCE]".to_owned());
    }
    let mut redact_next = false;
    input
        .split_whitespace()
        .map(|word| {
            if redact_next {
                redact_next = false;
                return "[REDACTED]";
            }
            let lower = word.to_ascii_lowercase();
            if lower == "bearer" {
                redact_next = true;
                return word;
            }
            if ["token=", "password=", "secret=", "api_key=", "apikey="]
                .iter()
                .any(|prefix| lower.starts_with(prefix))
            {
                word.split_once('=')
                    .map_or("Bearer [REDACTED]", |(prefix, _)| {
                        let _ = prefix;
                        "[REDACTED]"
                    })
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                let sensitive = key.to_ascii_lowercase().contains("token")
                    || key.to_ascii_lowercase().contains("secret")
                    || key.to_ascii_lowercase().contains("password")
                    || key.to_ascii_lowercase().contains("api_key")
                    || key.eq_ignore_ascii_case("authorization");
                if sensitive {
                    *child = serde_json::Value::String("[REDACTED]".to_owned());
                } else {
                    redact_json(child);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json),
        _ => {}
    }
}

/// Runs one investigation and persists its redacted lifecycle facts.
#[derive(Debug)]
pub struct ShadowInvestigator {
    grafana: GrafanaContext,
    journal: JournalStore,
}

impl ShadowInvestigator {
    /// Creates a shadow investigator from read-only Grafana and journal bounds.
    pub fn new(grafana: GrafanaContext, journal: JournalStore) -> Self {
        Self { grafana, journal }
    }

    /// Collects evidence, runs finite provider fallback, and persists facts.
    pub async fn investigate(
        &mut self,
        signal: &IncidentSignal,
        runtime: &mut IncidentRuntime,
        providers: &mut [RecordedProvider],
        reservation: Reservation,
        queries: InvestigationQueries,
        start_at_ms: u64,
    ) -> Result<InvestigationResult, InvestigationError> {
        self.journal.append(JournalEvent::IncidentOpened {
            incident_id: signal.incident_id.clone(),
            alert_name: signal.alert_name.clone(),
        })?;
        self.journal.append(JournalEvent::PhaseStarted {
            phase: Phase::Investigation,
            at_ms: start_at_ms,
        })?;

        let mut collected = super::evidence::EvidenceBoard::default();
        self.grafana.logs(&mut collected, queries.logs).await?;
        self.grafana
            .metrics(&mut collected, queries.metrics)
            .await?;
        for record in collected.records() {
            runtime.commit_evidence(
                record.source,
                record.query.clone(),
                record.payload.clone(),
                start_at_ms,
            )?;
        }
        self.journal.append(JournalEvent::PhaseFinished {
            phase: Phase::Investigation,
            at_ms: start_at_ms,
        })?;
        let status = runtime.run_recorded(providers, reservation, start_at_ms)?;
        for entry in runtime.journal().entries() {
            self.journal.append(entry.event.clone())?;
        }

        Ok(InvestigationResult {
            incident_id: signal.incident_id.clone(),
            evidence_ids: runtime
                .evidence()
                .records()
                .iter()
                .map(|record| record.evidence_id.clone())
                .collect(),
            status,
            report: runtime.last_report().cloned().unwrap_or_else(|| {
                super::contracts::DiagnosticReport {
                    summary:
                        "No provider produced an accepted diagnosis within the incident budget."
                            .to_owned(),
                    evidence: Vec::new(),
                }
            }),
        })
    }

    /// Exposes the durable journal for replay and reporting.
    pub fn journal(&self) -> &JournalStore {
        &self.journal
    }

    /// Builds conservative queries from an explicitly selected service name.
    pub fn queries_for_service(service: &str) -> Option<InvestigationQueries> {
        if service.is_empty()
            || !service.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
            })
        {
            return None;
        }
        Some(InvestigationQueries {
            logs: format!(r#"{{service="{service}"}} |= "error""#),
            metrics: format!(r#"up{{service="{service}"}}"#),
        })
    }

    /// Returns the provider that produced a successful report, if any.
    pub fn terminal_provider(result: &InvestigationResult) -> Option<ProviderKind> {
        match result.status {
            super::coordinator::RunStatus::Succeeded(provider) => Some(provider),
            super::coordinator::RunStatus::Exhausted => None,
        }
    }
}
