//! Live provider execution over the bounded reasoning runtime.
//!
//! Vendor adapters remain outside this module. This runner only selects the
//! admitted provider, converts results into safe attempt facts, validates
//! evidence citations, and advances finite fallback.

use std::{future::Future, pin::Pin, time::Instant};
use tokio::time::timeout;

use super::{
    baseline::build_report,
    budget::Reservation,
    contracts::DiagnosticReport,
    coordinator::{AttemptFacts, CoordinatorError, FailureClass, RunStatus},
    router::ProviderKind,
    runtime::{IncidentRuntime, RuntimeError},
};

/// Optional live providers configured for one process.
pub struct LiveProviders<'a> {
    /// OpenAI capability, if its live gate is accepted.
    pub openai: Option<&'a dyn LiveProvider>,
    /// Gemini capability, if its live gate is accepted.
    pub gemini: Option<&'a dyn LiveProvider>,
    /// DeepSeek capability, if its live gate is accepted.
    pub deepseek: Option<&'a dyn LiveProvider>,
}

/// Provider-neutral completion future returned by adapter capabilities.
pub type LiveCompletion<'a> = Pin<
    Box<dyn Future<Output = Result<(DiagnosticReport, Option<u64>), FailureClass>> + Send + 'a>,
>;

/// Core-owned provider capability; vendor adapters implement this boundary.
pub trait LiveProvider: Sync {
    /// Completes one bounded prompt and returns provider-neutral facts.
    fn complete<'a>(&'a self, prompt: &'a str) -> LiveCompletion<'a>;
}

/// Executes the configured provider order with durable budget admission.
pub async fn run_live(
    runtime: &mut IncidentRuntime,
    providers: LiveProviders<'_>,
    prompt: &str,
    reservation: Reservation,
    start_at_ms: u64,
) -> Result<RunStatus, RuntimeError> {
    // Evidence is collected once before provider fallback; do not charge it
    // repeatedly to every model attempt.
    let evidence_queries = 0;
    let evidence_ids = runtime
        .evidence()
        .records()
        .iter()
        .map(|record| record.evidence_id.clone())
        .collect();
    let mut at_ms = start_at_ms;

    loop {
        let provider = match runtime.admit_provider(reservation, at_ms) {
            Ok(Some(provider)) => provider,
            Ok(None) => return Ok(RunStatus::Exhausted),
            Err(RuntimeError::Coordination(CoordinatorError::Budget(_))) => {
                return Ok(RunStatus::Exhausted);
            }
            Err(error) => return Err(error),
        };
        let started = Instant::now();
        let result = if let Some(remaining) = runtime.remaining() {
            timeout(remaining, async {
                match provider {
                    ProviderKind::OpenAi => match providers.openai {
                        Some(provider) => provider.complete(prompt).await,
                        None => Err(FailureClass::TemporarilyUnavailable),
                    },
                    ProviderKind::Gemini => match providers.gemini {
                        Some(provider) => provider.complete(prompt).await,
                        None => Err(FailureClass::TemporarilyUnavailable),
                    },
                    ProviderKind::DeepSeek => match providers.deepseek {
                        Some(provider) => provider.complete(prompt).await,
                        None => Err(FailureClass::TemporarilyUnavailable),
                    },
                    ProviderKind::Deterministic => {
                        Ok((build_report("incident", &evidence_ids), None))
                    }
                }
            })
            .await
            .unwrap_or(Err(FailureClass::TemporarilyUnavailable))
        } else {
            Err(FailureClass::TemporarilyUnavailable)
        };
        let facts = AttemptFacts {
            elapsed_ms: started.elapsed().as_millis() as u64,
            tokens: result.as_ref().ok().and_then(|(_, tokens)| *tokens),
            evidence_queries,
            ..AttemptFacts::default()
        };
        match result {
            Ok((report, _)) => {
                if report
                    .validate_against(
                        &runtime
                            .evidence()
                            .records()
                            .iter()
                            .map(|record| record.evidence_id.clone())
                            .collect(),
                    )
                    .is_ok()
                {
                    return runtime.succeed_provider_with_report(provider, report, facts, at_ms);
                }
                runtime.fail_provider(provider, FailureClass::MalformedResponse, facts, at_ms)?;
            }
            Err(failure) => runtime.fail_provider(provider, failure, facts, at_ms)?,
        }
        at_ms = runtime.elapsed_ms();
    }
}
