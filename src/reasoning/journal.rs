//! Append-only incident facts and deterministic efficiency projection.
//!
//! The journal stores raw facts, not derived telemetry. Replaying the same
//! entries must produce the same timing, usage, provider-path, and cost view.

use serde::{Deserialize, Serialize};
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
    },
    /// Records a duplicate signal without starting another workflow.
    AlertDeduplicated {
        /// Existing incident identity.
        incident_id: String,
    },
    /// Records recovery for an existing incident.
    IncidentRecovered {
        /// Stable incident identity.
        incident_id: String,
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
    /// Stores the terminal provider outcome.
    Terminal {
        /// Provider that produced the terminal report, if any.
        provider: Option<ProviderKind>,
    },
}

/// A journal entry with a monotonic sequence assigned by the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Durable ordering key, independent of wall-clock timestamps.
    pub sequence: u64,
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
        self.entries.push(JournalEntry { sequence, event });
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
                | JournalEvent::AlertDeduplicated { incident_id }
                | JournalEvent::IncidentRecovered { incident_id } => Some(incident_id.clone()),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn restore(&mut self, entry: JournalEntry) {
        self.entries.push(entry);
    }

    /// Rebuilds efficiency metrics from raw facts only.
    pub fn project(&self) -> EfficiencyProjection {
        let mut projection = EfficiencyProjection::default();
        let mut phase_starts = [
            (Phase::Investigation, None),
            (Phase::Reasoning, None),
            (Phase::HumanWait, None),
            (Phase::Execution, None),
        ];
        for entry in &self.entries {
            match &entry.event {
                JournalEvent::IncidentOpened { .. }
                | JournalEvent::AlertDeduplicated { .. }
                | JournalEvent::IncidentRecovered { .. }
                | JournalEvent::EvidenceCommitted { .. } => {}
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
