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
#[serde(deny_unknown_fields)]
pub struct ProviderOrder {
    /// Providers attempted in order; each run restarts from the evidence board.
    pub providers: Vec<ProviderKind>,
}

/// Identity of one complete, isolated provider reasoning run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderRun {
    /// Provider selected for this complete run.
    pub provider: ProviderKind,
    /// Monotonic generation used to prevent mixed-run state.
    pub generation: u32,
}

/// Pure fallback state that preserves attempted providers across restarts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderRunState {
    attempted: BTreeSet<ProviderKind>,
    generation: u32,
}

/// Versioned admission set for providers that have passed their live gates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAdmission {
    admitted: BTreeSet<ProviderKind>,
}

impl ProviderAdmission {
    /// Creates an admission policy from providers independently qualified for shadow use.
    pub fn new(admitted: impl IntoIterator<Item = ProviderKind>) -> Self {
        Self {
            admitted: admitted.into_iter().collect(),
        }
    }

    /// Returns whether a provider may be selected by the fallback router.
    pub fn allows(&self, provider: ProviderKind) -> bool {
        self.admitted.contains(&provider)
    }
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

impl ProviderRunState {
    /// Starts the next complete provider run without reusing a prior provider.
    pub fn start_next(&mut self, order: &ProviderOrder) -> Option<ProviderRun> {
        let provider = next_provider_in(&self.attempted, order)?;
        self.attempted.insert(provider);
        self.generation = self.generation.saturating_add(1);
        Some(ProviderRun {
            provider,
            generation: self.generation,
        })
    }

    /// Starts the next provider that is both unattempted and independently admitted.
    pub fn start_next_admitted(
        &mut self,
        order: &ProviderOrder,
        admission: &ProviderAdmission,
    ) -> Option<ProviderRun> {
        let provider =
            order.providers.iter().copied().find(|provider| {
                !self.attempted.contains(provider) && admission.allows(*provider)
            })?;
        self.attempted.insert(provider);
        self.generation = self.generation.saturating_add(1);
        Some(ProviderRun {
            provider,
            generation: self.generation,
        })
    }

    /// Returns whether a provider has already owned a complete run.
    pub fn attempted(&self, provider: ProviderKind) -> bool {
        self.attempted.contains(&provider)
    }

    /// Returns the current run generation.
    pub const fn generation(&self) -> u32 {
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::{ProviderKind, ProviderOrder, ProviderRunState};

    #[test]
    fn fallback_restarts_with_a_fresh_provider_run_generation() {
        // Given the configured OpenAI, Gemini, and deterministic fallback order.
        let order = ProviderOrder {
            providers: vec![
                ProviderKind::OpenAi,
                ProviderKind::Gemini,
                ProviderKind::Deterministic,
            ],
        };
        let mut state = ProviderRunState::default();

        // When the first provider fails and the next run is selected.
        let first = state.start_next(&order).expect("OpenAI run");
        let second = state.start_next(&order).expect("Gemini run");

        // Then the fallback cannot reuse provider state or generation identity.
        assert_eq!(first.provider, ProviderKind::OpenAi);
        assert_eq!(second.provider, ProviderKind::Gemini);
        assert_ne!(first.generation, second.generation);
        assert!(state.attempted(ProviderKind::OpenAi));
    }

    #[test]
    fn fallback_skips_providers_without_an_independent_admission_gate() {
        // Given an order where only deterministic reasoning has passed its gate.
        let order = ProviderOrder::default();
        let admission = super::ProviderAdmission::new([ProviderKind::Deterministic]);
        let mut state = ProviderRunState::default();

        // When the router selects the next admitted provider.
        let run = state
            .start_next_admitted(&order, &admission)
            .expect("deterministic fallback");

        // Then unauthenticated paid providers are never selected.
        assert_eq!(run.provider, ProviderKind::Deterministic);
    }
}
