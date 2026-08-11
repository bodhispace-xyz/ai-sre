//! Application boundary joining evidence, bounded reasoning, and journaling.
//!
//! Adapters call this service with already-classified results. The service owns
//! sequencing and durable facts but performs no provider or Grafana I/O.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};
use thiserror::Error;

use super::{
    budget::Reservation,
    contracts::DiagnosticReport,
    coordinator::{
        AttemptFacts, CoordinatorError, FailureClass, ReasoningConfig, ReasoningRun, RunStatus,
    },
    evidence::{EvidenceBoard, EvidenceError, EvidenceSource},
    journal::{IncidentJournal, JournalEvent, Phase, attempt_event},
    recorded::{RecordedOutcome, RecordedProvider},
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
    last_report: Option<DiagnosticReport>,
    started_at: Instant,
    deadline: Instant,
}

impl IncidentRuntime {
    /// Creates a fresh runtime from versioned policy and budget configuration.
    pub fn new(config: ReasoningConfig) -> Result<Self, RuntimeError> {
        let wall_time = Duration::from_secs(u64::from(config.budget.max_wall_time_secs));
        let started_at = Instant::now();
        Ok(Self {
            run: ReasoningRun::new(config)?,
            evidence: EvidenceBoard::default(),
            journal: IncidentJournal::default(),
            last_report: None,
            started_at,
            deadline: started_at + wall_time,
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
        if self.run.next_provider() == Some(ProviderKind::Deterministic) {
            return Ok(self.run.admit_deterministic()?);
        }
        Ok(self.run.admit(reservation)?)
    }

    /// Returns the remaining incident wall-clock allowance.
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline.checked_duration_since(Instant::now())
    }

    /// Returns elapsed monotonic milliseconds for journal boundaries.
    pub fn elapsed_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
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

    /// Accepts a validated provider report and records it as the terminal result.
    pub fn succeed_provider_with_report(
        &mut self,
        provider: ProviderKind,
        report: DiagnosticReport,
        facts: AttemptFacts,
        at_ms: u64,
    ) -> Result<RunStatus, RuntimeError> {
        let evidence_ids = self
            .evidence
            .records()
            .iter()
            .map(|record| record.evidence_id.clone())
            .collect::<BTreeSet<_>>();
        report
            .validate_against(&evidence_ids)
            .map_err(|_| RuntimeError::Evidence(EvidenceError::EmptyPayload))?;
        self.last_report = Some(report);
        self.succeed_provider(provider, facts, at_ms)
    }

    /// Executes a complete scripted fallback run for contract and shadow tests.
    ///
    /// Each provider starts from the same immutable evidence-board view; a
    /// failed or malformed response is discarded before the next provider.
    pub fn run_recorded(
        &mut self,
        providers: &mut [RecordedProvider],
        reservation: Reservation,
        start_at_ms: u64,
    ) -> Result<RunStatus, RuntimeError> {
        let evidence_ids = self
            .evidence
            .records()
            .iter()
            .map(|record| record.evidence_id.clone())
            .collect::<BTreeSet<_>>();
        let mut at_ms = start_at_ms;

        loop {
            let Some(provider_kind) = self.admit_provider(reservation, at_ms)? else {
                return Ok(self.run.status().unwrap_or(RunStatus::Exhausted));
            };
            let attempt = providers
                .iter_mut()
                .find(|provider| provider.provider() == provider_kind)
                .and_then(RecordedProvider::take_next);

            match attempt {
                Some(attempt) => match attempt.outcome {
                    RecordedOutcome::Success(json) => {
                        let report = DiagnosticReport::from_provider_json(&json);
                        let valid = report
                            .as_ref()
                            .is_ok_and(|report| report.validate_against(&evidence_ids).is_ok());
                        if valid {
                            self.last_report = report.ok();
                            return self.succeed_provider(provider_kind, attempt.facts, at_ms);
                        }
                        self.fail_provider(
                            provider_kind,
                            FailureClass::MalformedResponse,
                            attempt.facts,
                            at_ms,
                        )?;
                    }
                    RecordedOutcome::Failure(failure) => {
                        self.fail_provider(provider_kind, failure, attempt.facts, at_ms)?;
                    }
                },
                None => {
                    self.fail_provider(
                        provider_kind,
                        FailureClass::TemporarilyUnavailable,
                        AttemptFacts::default(),
                        at_ms,
                    )?;
                }
            }
            at_ms = at_ms.saturating_add(1);
        }
    }

    /// Returns the immutable evidence board for provider context export.
    pub fn evidence(&self) -> &EvidenceBoard {
        &self.evidence
    }

    /// Returns mutable evidence access for a capability adapter orchestrated by
    /// the shadow investigator. The adapter remains read-only and bounded.
    pub fn evidence_mut(&mut self) -> &mut EvidenceBoard {
        &mut self.evidence
    }

    /// Returns the append-only journal for persistence or replay.
    pub fn journal(&self) -> &IncidentJournal {
        &self.journal
    }

    /// Returns the accepted advisory report, if this run succeeded.
    pub fn last_report(&self) -> Option<&DiagnosticReport> {
        self.last_report.as_ref()
    }
}
