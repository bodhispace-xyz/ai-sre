//! Finite reasoning-run coordination over pure provider policy and budgets.
//!
//! The coordinator chooses one complete provider run at a time. It never
//! retries a partial response, performs I/O, or grants mutation authority.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    budget::{BudgetError, BudgetState, Reservation},
    router::{ProviderKind, ProviderOrder, next_provider_in},
};

/// Configurable inputs for one incident reasoning run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningConfig {
    /// Provider fallback order for complete restarts.
    pub provider_order: ProviderOrder,
    /// Per-incident resource ceilings.
    pub budget: super::budget::BudgetConfig,
    /// Maximum model-directed context turns per provider run.
    #[serde(default = "default_tool_turns")]
    pub max_tool_turns: u32,
}

const fn default_tool_turns() -> u32 {
    4
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        Self {
            provider_order: ProviderOrder::default(),
            budget: super::budget::BudgetConfig::default(),
            max_tool_turns: default_tool_turns(),
        }
    }
}

/// Safe terminal states of a reasoning run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// A provider has returned an accepted report.
    Succeeded(ProviderKind),
    /// Every configured provider was attempted without an accepted report.
    Exhausted,
}

/// Safe provider failure classes recorded in the incident journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureClass {
    /// Credentials must be reauthenticated before this provider can run.
    AuthenticationRequired,
    /// Provider quota or rate limit prevented the attempt.
    RateLimited,
    /// Transport or provider outage prevented the attempt.
    TemporarilyUnavailable,
    /// Provider output failed the strict response contract.
    MalformedResponse,
}

/// Measured facts captured for one provider attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AttemptFacts {
    /// End-to-end adapter time in milliseconds.
    pub elapsed_ms: u64,
    /// Trusted provider-reported token usage, when available.
    pub tokens: Option<u64>,
    /// Read-only evidence queries performed during this attempt.
    pub evidence_queries: u32,
    /// Estimated billed cost; `None` means cost is unknown.
    pub cost_micro_usd: Option<u64>,
}

/// Journal-ready result for one complete provider run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttemptOutcome {
    /// The provider returned an accepted report.
    Succeeded,
    /// The provider run failed and was discarded before fallback.
    Failed(FailureClass),
}

/// Immutable efficiency facts for one attempted provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRecord {
    /// Provider selected for this complete run.
    pub provider: ProviderKind,
    /// Safe terminal classification.
    pub outcome: AttemptOutcome,
    /// Measured facts that survive restart and can be projected into metrics.
    pub facts: AttemptFacts,
}

/// Errors that prevent admission of the next provider run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CoordinatorError {
    /// The provider order is invalid.
    #[error("invalid provider order: {0}")]
    InvalidOrder(&'static str),
    /// The run cannot reserve the requested worst-case operation.
    #[error("reasoning run budget rejected: {0}")]
    Budget(#[from] BudgetError),
    /// A provider completion was reported without an active provider.
    #[error("provider completion reported without an active run")]
    NoActiveRun,
    /// A provider completion did not match the active provider.
    #[error("provider completion does not match the active run")]
    WrongProvider,
}

/// Pure state machine for one bounded reasoning incident.
#[derive(Debug, Clone)]
pub struct ReasoningRun {
    order: ProviderOrder,
    budget: BudgetState,
    attempted: BTreeSet<ProviderKind>,
    active: Option<ProviderKind>,
    status: Option<RunStatus>,
    attempts: Vec<AttemptRecord>,
}

impl ReasoningRun {
    /// Creates a run and validates its configurable provider order.
    pub fn new(config: ReasoningConfig) -> Result<Self, CoordinatorError> {
        config
            .provider_order
            .validate()
            .map_err(CoordinatorError::InvalidOrder)?;
        Ok(Self {
            order: config.provider_order,
            budget: BudgetState::new(config.budget),
            attempted: BTreeSet::new(),
            active: None,
            status: None,
            attempts: Vec::new(),
        })
    }

    /// Reserves resources and admits exactly one next provider run.
    pub fn admit(
        &mut self,
        reservation: Reservation,
    ) -> Result<Option<ProviderKind>, CoordinatorError> {
        if self.status.is_some() {
            return Ok(None);
        }
        if self.active.is_some() {
            return Err(CoordinatorError::NoActiveRun);
        }
        let Some(provider) = next_provider_in(&self.attempted, &self.order) else {
            self.status = Some(RunStatus::Exhausted);
            return Ok(None);
        };
        self.budget.reserve(reservation)?;
        self.attempted.insert(provider);
        self.active = Some(provider);
        Ok(Some(provider))
    }

    /// Reserves one model-directed read-only evidence query.
    pub fn reserve_evidence_query(&mut self) -> Result<(), CoordinatorError> {
        Ok(self.budget.reserve(Reservation {
            provider_calls: 0,
            tokens: 0,
            evidence_queries: 1,
            cost_micro_usd: 0,
        })?)
    }

    /// Returns the next provider without consuming any budget.
    pub fn next_provider(&self) -> Option<ProviderKind> {
        if self.status.is_some() || self.active.is_some() {
            return None;
        }
        next_provider_in(&self.attempted, &self.order)
    }

    /// Admits the deterministic baseline without model/tool reservation.
    pub fn admit_deterministic(&mut self) -> Result<Option<ProviderKind>, CoordinatorError> {
        if self.next_provider() != Some(ProviderKind::Deterministic) {
            return Ok(None);
        }
        self.attempted.insert(ProviderKind::Deterministic);
        self.active = Some(ProviderKind::Deterministic);
        Ok(Some(ProviderKind::Deterministic))
    }

    /// Discards the failed complete run and permits the next fallback.
    pub fn fail(&mut self, provider: ProviderKind) -> Result<(), CoordinatorError> {
        self.fail_with(
            provider,
            FailureClass::TemporarilyUnavailable,
            AttemptFacts::default(),
        )
    }

    /// Records a classified failure and discards that complete provider run.
    pub fn fail_with(
        &mut self,
        provider: ProviderKind,
        failure: FailureClass,
        facts: AttemptFacts,
    ) -> Result<(), CoordinatorError> {
        match self.active.take() {
            Some(active) if active == provider => {
                self.attempts.push(AttemptRecord {
                    provider,
                    outcome: AttemptOutcome::Failed(failure),
                    facts,
                });
                Ok(())
            }
            Some(_) => Err(CoordinatorError::WrongProvider),
            None => Err(CoordinatorError::NoActiveRun),
        }
    }

    /// Commits an accepted report as the terminal run result.
    pub fn succeed(&mut self, provider: ProviderKind) -> Result<RunStatus, CoordinatorError> {
        self.succeed_with(provider, AttemptFacts::default())
    }

    /// Records successful usage facts and commits the accepted report.
    pub fn succeed_with(
        &mut self,
        provider: ProviderKind,
        facts: AttemptFacts,
    ) -> Result<RunStatus, CoordinatorError> {
        match self.active {
            Some(active) if active == provider => {
                self.attempts.push(AttemptRecord {
                    provider,
                    outcome: AttemptOutcome::Succeeded,
                    facts,
                });
                let status = RunStatus::Succeeded(provider);
                self.active = None;
                self.status = Some(status);
                Ok(status)
            }
            Some(_) => Err(CoordinatorError::WrongProvider),
            None => Err(CoordinatorError::NoActiveRun),
        }
    }

    /// Returns immutable accounting facts for the incident journal.
    pub fn budget_totals(&self) -> (u32, u64, u32, u64) {
        self.budget.totals()
    }

    /// Returns the configured per-incident evidence-query ceiling.
    pub const fn max_evidence_queries(&self) -> u32 {
        self.budget.max_evidence_queries()
    }

    /// Returns the terminal status, if the run has stopped.
    pub fn status(&self) -> Option<RunStatus> {
        self.status
    }

    /// Returns the append-only attempt facts for journal projection.
    pub fn attempts(&self) -> &[AttemptRecord] {
        &self.attempts
    }
}
