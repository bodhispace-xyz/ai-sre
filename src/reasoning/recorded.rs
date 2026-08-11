//! Deterministic provider harness for contract and shadow-mode tests.
//!
//! The harness scripts complete provider outcomes and usage facts without
//! network access, credentials, or vendor SDK types.

use std::collections::VecDeque;

use super::{
    coordinator::{AttemptFacts, FailureClass},
    router::ProviderKind,
};

/// One scripted outcome for a complete provider run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedOutcome {
    /// Provider returned a serialized provider-neutral report.
    Success(String),
    /// Provider failed with a safe adapter classification.
    Failure(FailureClass),
}

/// Scripted provider result with measured facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedAttempt {
    /// Complete-run outcome.
    pub outcome: RecordedOutcome,
    /// Usage and timing facts that the real adapter would report.
    pub facts: AttemptFacts,
}

/// A deterministic, finite provider implementation for tests and shadow mode.
#[derive(Debug, Clone)]
pub struct RecordedProvider {
    provider: ProviderKind,
    attempts: VecDeque<RecordedAttempt>,
}

impl RecordedProvider {
    /// Creates a provider with a finite scripted sequence.
    pub fn new(
        provider: ProviderKind,
        attempts: impl IntoIterator<Item = RecordedAttempt>,
    ) -> Self {
        Self {
            provider,
            attempts: attempts.into_iter().collect(),
        }
    }

    /// Returns the provider identity represented by this recording.
    pub const fn provider(&self) -> ProviderKind {
        self.provider
    }

    /// Consumes the next complete-run result, if one was recorded.
    pub fn take_next(&mut self) -> Option<RecordedAttempt> {
        self.attempts.pop_front()
    }
}
