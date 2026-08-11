//! Pure provider-order decisions for complete, restartable reasoning runs.
//!
//! This module records no effects and never mixes partial responses. The
//! imperative shell invokes exactly the provider selected by these decisions.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Providers in the fixed fallback order for one incident run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ProviderKind {
    /// OpenAI ChatGPT OAuth-backed reasoning.
    OpenAi,
    /// Gemini API reasoning.
    Gemini,
    /// DeepSeek API reasoning.
    DeepSeek,
    /// Local deterministic enrichment and baseline reasoning.
    Deterministic,
}

/// Configurable provider order for one complete reasoning run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderOrder {
    /// Providers attempted in order; each run restarts from the evidence board.
    pub providers: Vec<ProviderKind>,
}

impl Default for ProviderOrder {
    fn default() -> Self {
        Self {
            providers: vec![
                ProviderKind::OpenAi,
                ProviderKind::Gemini,
                ProviderKind::DeepSeek,
                ProviderKind::Deterministic,
            ],
        }
    }
}

impl ProviderOrder {
    /// Rejects duplicate providers and an empty fallback plan.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.providers.is_empty() {
            return Err("provider order cannot be empty");
        }
        let unique = self.providers.iter().collect::<BTreeSet<_>>();
        (unique.len() == self.providers.len())
            .then_some(())
            .ok_or("provider order cannot contain duplicates")
    }
}

/// Chooses the next unattempted provider in the configured order.
pub fn next_provider(attempted: &BTreeSet<ProviderKind>) -> Option<ProviderKind> {
    next_provider_in(attempted, &ProviderOrder::default())
}

/// Chooses the next unattempted provider from configurable policy.
pub fn next_provider_in(
    attempted: &BTreeSet<ProviderKind>,
    order: &ProviderOrder,
) -> Option<ProviderKind> {
    order
        .providers
        .iter()
        .copied()
        .find(|provider| !attempted.contains(provider))
}
