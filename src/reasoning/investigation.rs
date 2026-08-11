//! Shadow-mode investigation orchestration.
//!
//! This service sequences normalized incidents through bounded Grafana
//! evidence collection and the existing finite provider runtime. It records
//! lifecycle facts but never invokes the gateway or performs mutations.

use std::collections::BTreeSet;

use thiserror::Error;

use crate::adapters::grafana::context::{ContextError, GrafanaContext};

use super::{
    budget::Reservation,
    incident::IncidentSignal,
    journal::{JournalEvent, Phase},
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
}

/// Inputs for one live-provider investigation.
pub struct LiveInvestigationInput<'a> {
    /// Read-only Grafana context.
    pub grafana: &'a GrafanaContext,
    /// Single-writer durable journal.
    pub journal: &'a mut JournalStore,
    /// Normalized incident signal.
    pub signal: &'a IncidentSignal,
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
        runtime,
        providers,
        reservation,
        queries,
        start_at_ms,
    } = input;
    journal.append(JournalEvent::PhaseStarted {
        phase: Phase::Investigation,
        at_ms: start_at_ms,
    })?;
    let mut collected = super::evidence::EvidenceBoard::default();
    grafana.logs(&mut collected, queries.logs).await?;
    grafana.metrics(&mut collected, queries.metrics).await?;
    for record in collected.records() {
        runtime.commit_evidence(
            record.source,
            record.query.clone(),
            record.payload.clone(),
            start_at_ms,
        )?;
    }
    journal.append(JournalEvent::PhaseFinished {
        phase: Phase::Investigation,
        at_ms: start_at_ms,
    })?;
    let prompt = build_prompt(signal, runtime);
    let status = run_live(runtime, providers, &prompt, reservation, start_at_ms).await?;
    for entry in runtime.journal().entries() {
        journal.append(entry.event.clone())?;
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
        let payload = String::from_utf8_lossy(&record.payload);
        prompt.push_str(&format!(
            "Evidence {} ({:?}, query={}): {}\n",
            record.evidence_id, record.source, record.query, payload
        ));
        if prompt.len() >= 32_768 {
            prompt.truncate(32_768);
            break;
        }
    }
    prompt
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
