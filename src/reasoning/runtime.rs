//! Application boundary joining evidence, bounded reasoning, and journaling.
//!
//! Adapters call this service with already-classified results. The service owns
//! sequencing and durable facts but performs no provider or Grafana I/O.

use thiserror::Error;

use super::{
    budget::Reservation,
    coordinator::{
        AttemptFacts, CoordinatorError, FailureClass, ReasoningConfig, ReasoningRun, RunStatus,
    },
    evidence::{EvidenceBoard, EvidenceError, EvidenceSource},
    journal::{IncidentJournal, JournalEvent, Phase, attempt_event},
    router::ProviderKind,
};

/// Errors crossing the application service boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RuntimeError {
    /// The coordinator rejected a sequencing or budget operation.
    #[error("reasoning runtime coordination failed")]
    Coordination(#[from] CoordinatorError),
    /// The evidence board rejected a tool result.
    #[error("reasoning runtime evidence commit failed")]
    Evidence(#[from] EvidenceError),
}

/// In-process application service for one incident run.
#[derive(Debug, Clone)]
pub struct IncidentRuntime {
    run: ReasoningRun,
    evidence: EvidenceBoard,
    journal: IncidentJournal,
}

impl IncidentRuntime {
    /// Creates a fresh runtime from versioned policy and budget configuration.
    pub fn new(config: ReasoningConfig) -> Result<Self, RuntimeError> {
        Ok(Self {
            run: ReasoningRun::new(config)?,
            evidence: EvidenceBoard::default(),
            journal: IncidentJournal::default(),
        })
    }

    /// Commits one tool result and records its source and timestamp.
    pub fn commit_evidence(
        &mut self,
        source: EvidenceSource,
        query: impl Into<String>,
        payload: Vec<u8>,
        at_ms: u64,
    ) -> Result<String, RuntimeError> {
        let evidence_id = self.evidence.commit(source, query, payload)?;
        self.journal.append(JournalEvent::EvidenceCommitted {
            evidence_id: evidence_id.clone(),
            source,
            at_ms,
        });
        Ok(evidence_id)
    }

    /// Starts the reasoning phase and admits one provider after reservation.
    pub fn admit_provider(
        &mut self,
        reservation: Reservation,
        at_ms: u64,
    ) -> Result<Option<ProviderKind>, RuntimeError> {
        self.journal.append(JournalEvent::PhaseStarted {
            phase: Phase::Reasoning,
            at_ms,
        });
        Ok(self.run.admit(reservation)?)
    }

    /// Records a classified failed attempt and leaves the run eligible for fallback.
    pub fn fail_provider(
        &mut self,
        provider: ProviderKind,
        failure: FailureClass,
        facts: AttemptFacts,
        at_ms: u64,
    ) -> Result<(), RuntimeError> {
        self.run.fail_with(provider, failure, facts)?;
        let attempt = *self
            .run
            .attempts()
            .last()
            .expect("coordinator records failures");
        self.journal.append(attempt_event(attempt));
        self.journal.append(JournalEvent::PhaseFinished {
            phase: Phase::Reasoning,
            at_ms,
        });
        Ok(())
    }

    /// Records an accepted report and closes the run with its provider identity.
    pub fn succeed_provider(
        &mut self,
        provider: ProviderKind,
        facts: AttemptFacts,
        at_ms: u64,
    ) -> Result<RunStatus, RuntimeError> {
        let status = self.run.succeed_with(provider, facts)?;
        let attempt = *self
            .run
            .attempts()
            .last()
            .expect("coordinator records success");
        self.journal.append(attempt_event(attempt));
        self.journal.append(JournalEvent::PhaseFinished {
            phase: Phase::Reasoning,
            at_ms,
        });
        self.journal.append(JournalEvent::Terminal {
            provider: Some(provider),
        });
        Ok(status)
    }

    /// Returns the immutable evidence board for provider context export.
    pub fn evidence(&self) -> &EvidenceBoard {
        &self.evidence
    }

    /// Returns the append-only journal for persistence or replay.
    pub fn journal(&self) -> &IncidentJournal {
        &self.journal
    }
}
