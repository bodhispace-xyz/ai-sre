//! Append-only incident facts and deterministic efficiency projection.
//!
//! The journal stores raw facts, not derived telemetry. Replaying the same
//! entries must produce the same timing, usage, provider-path, and cost view.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

use super::coordinator::AttemptRecord;
use super::router::ProviderKind;

/// Durable lifecycle phases used for timing projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Incident intake and evidence collection.
    Investigation,
    /// Model reasoning and fallback attempts.
    Reasoning,
    /// Human approval wait.
    HumanWait,
    /// Runtime execution and verification.
    Execution,
}

/// Raw append-only fact for an incident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalEvent {
    /// Records the first signal for a normalized incident.
    IncidentOpened {
        /// Stable incident identity.
        incident_id: String,
        /// Normalized alert name.
        alert_name: String,
        /// Labels required to reconstruct a pending shadow investigation.
        #[serde(default)]
        labels: std::collections::BTreeMap<String, String>,
        /// Annotations retained for the pending investigation context.
        #[serde(default)]
        annotations: std::collections::BTreeMap<String, String>,
        /// Source event time used for stale-event ordering.
        #[serde(default)]
        event_time: String,
        /// Canonical source event identity.
        #[serde(default)]
        source_event_id: String,
    },
    /// Records a duplicate signal without starting another workflow.
    AlertDeduplicated {
        /// Existing incident identity.
        incident_id: String,
        /// Source event time used for exact replay identity.
        #[serde(default)]
        event_time: String,
        /// Canonical source event identity.
        #[serde(default)]
        source_event_id: String,
    },
    /// Records an older lifecycle event without allowing state regression.
    AlertOutOfOrder {
        /// Incident identity from the stale source event.
        incident_id: String,
        /// Lifecycle status carried by the stale event.
        status: super::incident::AlertStatus,
        /// Source event time that was rejected for ordering.
        event_time: String,
        /// Canonical source event identity.
        #[serde(default)]
        source_event_id: String,
    },
    /// Records recovery for an existing incident.
    IncidentRecovered {
        /// Stable incident identity.
        incident_id: String,
        /// Source event time used for stale-event ordering.
        #[serde(default)]
        event_time: String,
        /// Canonical source event identity.
        #[serde(default)]
        source_event_id: String,
    },
    /// Records that the incident workflow reached a terminal report state.
    IncidentCompleted {
        /// Stable incident episode identity.
        incident_id: String,
    },
    /// Records restart/redelivery resumption of an incomplete episode.
    IncidentResumed {
        /// Stable incident episode identity.
        incident_id: String,
        /// Source event time that caused the resume.
        #[serde(default)]
        event_time: String,
        /// Canonical source event identity.
        #[serde(default)]
        source_event_id: String,
    },
    /// Records an evidence-board commit without storing credentials.
    EvidenceCommitted {
        /// Stable evidence identifier.
        evidence_id: String,
        /// Source capability that produced the record.
        source: super::evidence::EvidenceSource,
        /// Monotonic commit timestamp in milliseconds.
        at_ms: u64,
    },
    /// Marks the beginning of a phase at an injected monotonic timestamp.
    PhaseStarted {
        /// Phase that started.
        phase: Phase,
        /// Monotonic timestamp in milliseconds.
        at_ms: u64,
    },
    /// Marks the end of a phase at an injected monotonic timestamp.
    PhaseFinished {
        /// Phase that finished.
        phase: Phase,
        /// Monotonic timestamp in milliseconds.
        at_ms: u64,
    },
    /// Stores one complete provider-attempt result.
    ProviderAttempt(AttemptRecord),
    /// Records an authorized context request before external I/O begins.
    ToolRequested {
        /// Provider that requested the capability.
        provider: ProviderKind,
        /// Provider correlation identifier.
        call_id: String,
        /// Allowlisted capability name.
        tool: String,
        /// Stable non-secret query digest.
        query_digest: String,
        /// Reserved evidence-query count before this request.
        evidence_queries_before: u32,
        /// Monotonic request timestamp.
        at_ms: u64,
    },
    /// Stores one bounded model-directed context request and its result.
    ToolContext {
        /// Provider that requested the context capability.
        provider: ProviderKind,
        /// Provider correlation identifier.
        call_id: String,
        /// Allowlisted capability name, never a raw command.
        tool: String,
        /// Stable non-secret digest of the query expression.
        query_digest: String,
        /// Safe result classification from the context adapter.
        result_class: super::tools::ToolResultClass,
        /// Monotonic elapsed time for the context call.
        elapsed_ms: u64,
        /// Bounded bytes committed or returned by the adapter.
        output_bytes: u64,
        /// Reserved query count before this call.
        evidence_queries_before: u32,
        /// Reserved query count after this call.
        evidence_queries_after: u32,
        /// Monotonic timestamp relative to the incident epoch.
        at_ms: u64,
    },
    /// Stores the terminal provider outcome.
    Terminal {
        /// Provider that produced the terminal report, if any.
        provider: Option<ProviderKind>,
    },
}

/// Bounded facts captured for one model-directed context call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolContextFacts {
    /// Provider that requested the context capability.
    pub provider: ProviderKind,
    /// Provider correlation identifier.
    pub call_id: String,
    /// Allowlisted capability name, never a raw command.
    pub tool: String,
    /// Stable non-secret digest of the query expression.
    pub query_digest: String,
    /// Safe result classification from the context adapter.
    pub result_class: super::tools::ToolResultClass,
    /// Monotonic elapsed time for the context call.
    pub elapsed_ms: u64,
    /// Bounded bytes committed or returned by the adapter.
    pub output_bytes: u64,
    /// Reserved query count before this call.
    pub evidence_queries_before: u32,
    /// Reserved query count after this call.
    pub evidence_queries_after: u32,
    /// Monotonic timestamp relative to the incident epoch.
    pub at_ms: u64,
}

/// Stable scope attached to every persisted causal fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalContext {
    /// Incident episode identity.
    pub incident_id: String,
    /// Reasoning-run identity within the incident episode.
    pub run_id: String,
}

/// A journal entry with a monotonic sequence assigned by the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Durable ordering key, independent of wall-clock timestamps.
    pub sequence: u64,
    /// Optional durable scope for facts written by the application shell.
    #[serde(default)]
    pub context: Option<JournalContext>,
    /// Raw incident fact.
    pub event: JournalEvent,
}

/// In-memory append-only journal used by the core; a storage adapter persists it.
#[derive(Debug, Clone, Default)]
pub struct IncidentJournal {
    entries: Vec<JournalEntry>,
}

impl IncidentJournal {
    /// Appends one event and assigns the next sequence.
    pub fn append(&mut self, event: JournalEvent) {
        let sequence = self.entries.len() as u64;
        self.entries.push(JournalEntry {
            sequence,
            context: None,
            event,
        });
    }

    /// Returns entries in sequence order for durable persistence or replay.
    pub fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }

    /// Returns incident identities already represented in the journal.
    pub fn incident_ids(&self) -> BTreeSet<String> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.event {
                JournalEvent::IncidentOpened { incident_id, .. }
                | JournalEvent::AlertDeduplicated { incident_id, .. }
                | JournalEvent::AlertOutOfOrder { incident_id, .. }
                | JournalEvent::IncidentRecovered { incident_id, .. } => Some(incident_id.clone()),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn restore(&mut self, entry: JournalEntry) {
        self.entries.push(entry);
    }

    /// Rebuilds efficiency metrics from raw facts only.
    pub fn project(&self) -> EfficiencyProjection {
        self.project_matching(|_| true)
    }

    /// Rebuilds one incident/run projection from raw scoped facts only.
    pub fn project_scoped(&self, context: &JournalContext) -> EfficiencyProjection {
        self.project_matching(|entry| entry.context.as_ref() == Some(context))
    }

    fn project_matching(&self, include: impl Fn(&JournalEntry) -> bool) -> EfficiencyProjection {
        let mut projection = EfficiencyProjection::default();
        let mut phase_starts = [
            (Phase::Investigation, None),
            (Phase::Reasoning, None),
            (Phase::HumanWait, None),
            (Phase::Execution, None),
        ];
        for entry in &self.entries {
            if !include(entry) {
                continue;
            }
            match &entry.event {
                JournalEvent::IncidentOpened { .. }
                | JournalEvent::AlertDeduplicated { .. }
                | JournalEvent::AlertOutOfOrder { .. }
                | JournalEvent::IncidentRecovered { .. }
                | JournalEvent::IncidentCompleted { .. }
                | JournalEvent::IncidentResumed { .. }
                | JournalEvent::EvidenceCommitted { .. } => {}
                JournalEvent::ToolRequested { .. } => {}
                JournalEvent::ToolContext {
                    elapsed_ms,
                    output_bytes,
                    result_class,
                    ..
                } => {
                    projection.tool_calls += 1;
                    projection.tool_elapsed_ms += *elapsed_ms;
                    projection.tool_output_bytes += *output_bytes;
                    if *result_class == super::tools::ToolResultClass::Succeeded {
                        projection.successful_tool_calls += 1;
                    }
                }
                JournalEvent::PhaseStarted { phase, at_ms } => {
                    if let Some(slot) = phase_starts.iter_mut().find(|(item, _)| item == phase) {
                        slot.1 = Some(*at_ms);
                    }
                }
                JournalEvent::PhaseFinished { phase, at_ms } => {
                    if let Some((_, Some(start))) =
                        phase_starts.iter().find(|(item, _)| item == phase)
                    {
                        let duration = at_ms.saturating_sub(*start);
                        match phase {
                            Phase::HumanWait => projection.human_wait_ms += duration,
                            _ => projection.active_machine_ms += duration,
                        }
                        projection.end_to_end_ms = projection.end_to_end_ms.max(*at_ms);
                    }
                }
                JournalEvent::ProviderAttempt(attempt) => projection.add_attempt(*attempt),
                JournalEvent::Terminal { provider } => projection.terminal_provider = *provider,
            }
        }
        projection
    }
}

/// Replayable efficiency metrics derived from journal facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EfficiencyProjection {
    /// Latest completed phase timestamp relative to the incident epoch.
    pub end_to_end_ms: u64,
    /// Sum of non-human phase durations.
    pub active_machine_ms: u64,
    /// Sum of provider adapter durations.
    pub provider_time_ms: u64,
    /// Sum of human approval wait.
    pub human_wait_ms: u64,
    /// Number of provider attempts.
    pub provider_attempts: u32,
    /// Total evidence queries reported by attempts.
    pub evidence_queries: u32,
    /// Total trusted token usage.
    pub tokens: u64,
    /// Sum of known estimated costs only.
    pub known_cost_micro_usd: u64,
    /// Number of attempts whose cost was unknown.
    pub unknown_cost_attempts: u32,
    /// Number of model-directed context calls.
    pub tool_calls: u32,
    /// Number of context calls that committed evidence.
    pub successful_tool_calls: u32,
    /// Sum of bounded context-call durations.
    pub tool_elapsed_ms: u64,
    /// Sum of bounded result bytes retained for provider resumption.
    pub tool_output_bytes: u64,
    /// Terminal provider, if one was recorded.
    pub terminal_provider: Option<ProviderKind>,
}

impl EfficiencyProjection {
    fn add_attempt(&mut self, attempt: AttemptRecord) {
        self.provider_attempts += 1;
        self.provider_time_ms += attempt.facts.elapsed_ms;
        self.evidence_queries += attempt.facts.evidence_queries;
        self.tokens += attempt.facts.tokens.unwrap_or_default();
        match attempt.facts.cost_micro_usd {
            Some(cost) => self.known_cost_micro_usd += cost,
            None => self.unknown_cost_attempts += 1,
        }
    }
}

/// Converts an adapter-neutral attempt into a journal event.
pub fn attempt_event(attempt: AttemptRecord) -> JournalEvent {
    JournalEvent::ProviderAttempt(attempt)
}

/// Produces a stable, non-secret digest suitable for journal correlation.
pub fn query_digest(query: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ai-sre/query-digest/v1\0");
    hasher.update(query.as_bytes());
    let digest = hasher.finalize();
    let encoded = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("v1-sha256:{encoded}")
}
