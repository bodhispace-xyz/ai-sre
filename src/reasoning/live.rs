//! Live provider execution over the bounded reasoning runtime.
//!
//! Vendor adapters remain outside this module. This runner only selects the
//! admitted provider, converts results into safe attempt facts, validates
//! evidence citations, and advances finite fallback.

use std::time::Instant;
use tokio::time::timeout;

use crate::adapters::llm::{
    deepseek::DeepSeekClient,
    gemini::GeminiClient,
    openai::{AuthCache, OpenAiOAuth, refresh_and_complete},
};

use super::{
    budget::Reservation,
    contracts::{DiagnosticReport, EvidenceRef},
    coordinator::{AttemptFacts, CoordinatorError, FailureClass, RunStatus},
    router::ProviderKind,
    runtime::{IncidentRuntime, RuntimeError},
};

/// Optional live providers configured for one process.
pub struct LiveProviders<'a> {
    /// OpenAI OAuth and rotating refresh-token cache.
    pub openai: Option<(&'a OpenAiOAuth, &'a AuthCache)>,
    /// Gemini API client.
    pub gemini: Option<&'a GeminiClient>,
    /// DeepSeek API client.
    pub deepseek: Option<&'a DeepSeekClient>,
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
    let evidence_id = runtime
        .evidence()
        .records()
        .first()
        .map(|record| record.evidence_id.clone());
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
                    ProviderKind::OpenAi => run_openai(providers.openai, prompt).await,
                    ProviderKind::Gemini => run_gemini(providers.gemini, prompt).await,
                    ProviderKind::DeepSeek => run_deepseek(providers.deepseek, prompt).await,
                    ProviderKind::Deterministic => Ok((
                        DiagnosticReport {
                            summary: "No live provider was available; deterministic enrichment is required."
                                .to_owned(),
                            evidence: evidence_id
                                .clone()
                                .into_iter()
                                .map(|evidence_id| EvidenceRef { evidence_id })
                                .collect(),
                        },
                        None,
                    )),
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

async fn run_openai(
    provider: Option<(&OpenAiOAuth, &AuthCache)>,
    prompt: &str,
) -> Result<(DiagnosticReport, Option<u64>), FailureClass> {
    match provider {
        Some((oauth, cache)) => refresh_and_complete(oauth, cache, prompt).await,
        None => Err(FailureClass::TemporarilyUnavailable),
    }
}

async fn run_gemini(
    provider: Option<&GeminiClient>,
    prompt: &str,
) -> Result<(DiagnosticReport, Option<u64>), FailureClass> {
    match provider {
        Some(client) => client
            .complete_for_runtime(prompt)
            .await
            .map(|(report, facts)| (report, facts.tokens)),
        None => Err(FailureClass::TemporarilyUnavailable),
    }
}

async fn run_deepseek(
    provider: Option<&DeepSeekClient>,
    prompt: &str,
) -> Result<(DiagnosticReport, Option<u64>), FailureClass> {
    match provider {
        Some(client) => client
            .complete_for_runtime(prompt)
            .await
            .map(|(report, facts)| (report, facts.tokens)),
        None => Err(FailureClass::TemporarilyUnavailable),
    }
}
