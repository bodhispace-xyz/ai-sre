//! Pure, conservative incident budget reservation and reconciliation.
//!
//! Values are integer units so accounting is deterministic and cannot turn a
//! missing provider price into an apparent zero-cost run.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Configurable ceilings for one incident reasoning run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    /// Maximum provider calls, including fallback restarts.
    pub max_provider_calls: u32,
    /// Maximum model tokens reserved across all calls.
    pub max_tokens: u64,
    /// Maximum read-only evidence queries.
    pub max_evidence_queries: u32,
    /// Hard wall-clock ceiling in seconds.
    pub max_wall_time_secs: u32,
    /// Optional maximum estimated billed cost in integer micro-USD.
    pub max_cost_micro_usd: Option<u64>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_provider_calls: 4,
            max_tokens: 32_000,
            max_evidence_queries: 24,
            max_wall_time_secs: 300,
            max_cost_micro_usd: Some(2_000_000),
        }
    }
}

/// Remaining incident budget after prior reservations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetState {
    config: BudgetConfig,
    provider_calls: u32,
    tokens: u64,
    evidence_queries: u32,
    reserved_cost_micro_usd: u64,
}

/// Reservation request made before an external call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    /// Provider calls reserved by this operation.
    pub provider_calls: u32,
    /// Tokens reserved by this operation.
    pub tokens: u64,
    /// Evidence queries reserved by this operation.
    pub evidence_queries: u32,
    /// Estimated cost reserved by this operation.
    pub cost_micro_usd: u64,
}

/// Reasons an operation cannot be admitted under the incident budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum BudgetError {
    /// A requested amount exceeds the remaining provider-call ceiling.
    #[error("provider-call budget exhausted")]
    ProviderCalls,
    /// A requested amount exceeds the remaining token ceiling.
    #[error("token budget exhausted")]
    Tokens,
    /// A requested amount exceeds the remaining evidence-query ceiling.
    #[error("evidence-query budget exhausted")]
    EvidenceQueries,
    /// A requested amount exceeds the configured billed-cost ceiling.
    #[error("cost budget exhausted")]
    Cost,
}

impl BudgetState {
    /// Starts a fresh incident budget from configuration.
    pub fn new(config: BudgetConfig) -> Self {
        Self {
            config,
            provider_calls: 0,
            tokens: 0,
            evidence_queries: 0,
            reserved_cost_micro_usd: 0,
        }
    }

    /// Reserves the complete worst-case operation before invoking an adapter.
    pub fn reserve(&mut self, reservation: Reservation) -> Result<(), BudgetError> {
        if self
            .provider_calls
            .saturating_add(reservation.provider_calls)
            > self.config.max_provider_calls
        {
            return Err(BudgetError::ProviderCalls);
        }
        if self.tokens.saturating_add(reservation.tokens) > self.config.max_tokens {
            return Err(BudgetError::Tokens);
        }
        if self
            .evidence_queries
            .saturating_add(reservation.evidence_queries)
            > self.config.max_evidence_queries
        {
            return Err(BudgetError::EvidenceQueries);
        }
        if self.config.max_cost_micro_usd.is_some_and(|limit| {
            self.reserved_cost_micro_usd
                .saturating_add(reservation.cost_micro_usd)
                > limit
        }) {
            return Err(BudgetError::Cost);
        }

        self.provider_calls += reservation.provider_calls;
        self.tokens += reservation.tokens;
        self.evidence_queries += reservation.evidence_queries;
        self.reserved_cost_micro_usd += reservation.cost_micro_usd;
        Ok(())
    }

    /// Exposes immutable accounting facts for journaling and metrics.
    pub fn totals(&self) -> (u32, u64, u32, u64) {
        (
            self.provider_calls,
            self.tokens,
            self.evidence_queries,
            self.reserved_cost_micro_usd,
        )
    }

    /// Returns the configured evidence-query ceiling.
    pub const fn max_evidence_queries(&self) -> u32 {
        self.config.max_evidence_queries
    }
}
