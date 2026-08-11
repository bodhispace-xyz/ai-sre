//! Pure provider-order decisions for complete, restartable reasoning runs.
//!
//! This module records no effects and never mixes partial responses. The
//! imperative shell invokes exactly the provider selected by these decisions.

use std::collections::BTreeSet;

/// Providers in the fixed fallback order for one incident run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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

/// Chooses the next unattempted provider in the configured order.
pub fn next_provider(attempted: &BTreeSet<ProviderKind>) -> Option<ProviderKind> {
    [
        ProviderKind::OpenAi,
        ProviderKind::Gemini,
        ProviderKind::DeepSeek,
        ProviderKind::Deterministic,
    ]
    .into_iter()
    .find(|provider| !attempted.contains(provider))
}
