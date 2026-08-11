//! Finite reasoning-run coordination over pure provider policy and budgets.
//!
//! The coordinator chooses one complete provider run at a time. It never
//! retries a partial response, performs I/O, or grants mutation authority.

use std::collections::BTreeSet;

use thiserror::Error;

use super::{
    budget::{BudgetError, BudgetState, Reservation},
    router::{ProviderKind, ProviderOrder, next_provider_in},
};

/// Configurable inputs for one incident reasoning run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReasoningConfig {
    /// Provider fallback order for complete restarts.
    pub provider_order: ProviderOrder,
    /// Per-incident resource ceilings.
    pub budget: super::budget::BudgetConfig,
}

/// Safe terminal states of a reasoning run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// A provider has returned an accepted report.
    Succeeded(ProviderKind),
    /// Every configured provider was attempted without an accepted report.
    Exhausted,
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

    /// Discards the failed complete run and permits the next fallback.
    pub fn fail(&mut self, provider: ProviderKind) -> Result<(), CoordinatorError> {
        match self.active.take() {
            Some(active) if active == provider => Ok(()),
            Some(_) => Err(CoordinatorError::WrongProvider),
            None => Err(CoordinatorError::NoActiveRun),
        }
    }

    /// Commits an accepted report as the terminal run result.
    pub fn succeed(&mut self, provider: ProviderKind) -> Result<RunStatus, CoordinatorError> {
        match self.active {
            Some(active) if active == provider => {
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

    /// Returns the terminal status, if the run has stopped.
    pub fn status(&self) -> Option<RunStatus> {
        self.status
    }
}
