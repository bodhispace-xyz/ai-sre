//! Shadow-mode investigation orchestration.
//!
//! This service sequences normalized incidents through bounded Grafana
//! evidence collection and the existing finite provider runtime. It records
//! lifecycle facts but never invokes the gateway or performs mutations.

use std::collections::BTreeSet;
use tokio::time::timeout;

use thiserror::Error;

use crate::adapters::grafana::context::{
    ContextBudget, ContextError, GrafanaContext, ReadOnlyRequest,
};

use super::{
    budget::Reservation,
    incident::IncidentSignal,
    journal::{JournalContext, JournalEvent, Phase},
    live::LiveProviders,
    recorded::RecordedProvider,
    router::ProviderKind,
    runtime::{IncidentRuntime, RuntimeError},
    storage::{JournalStore, JournalStoreError},
    tools::{ToolCall, ToolLoop, ToolLoopError, ToolResult, ToolResultClass},
};

/// Explicit read-only queries selected for one shadow investigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvestigationQueries {
    /// LogQL expression for Loki.
    pub logs: String,
    /// PromQL expression for Prometheus.
    pub metrics: String,
}

/// Result of one bounded shadow investigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvestigationResult {
    /// Stable incident identity.
    pub incident_id: String,
    /// Evidence IDs available to the provider report.
    pub evidence_ids: BTreeSet<String>,
    /// Terminal provider state.
    pub status: super::coordinator::RunStatus,
    /// Accepted provider report, or a deterministic exhaustion explanation.
    pub report: super::contracts::DiagnosticReport,
}

/// Failures that stop a shadow investigation before mutation is possible.
#[derive(Debug, Error)]
pub enum InvestigationError {
    /// Grafana returned no usable bounded evidence.
    #[error("shadow investigation could not collect Grafana evidence")]
    Context(#[from] ContextError),
    /// The reasoning runtime rejected the workflow transition.
    #[error("shadow investigation reasoning failed")]
    Runtime(#[from] RuntimeError),
    /// The durable journal could not record a lifecycle fact.
    #[error("shadow investigation journal write failed")]
    Journal(#[from] JournalStoreError),
    /// The incident wall-clock budget expired before completion.
    #[error("shadow investigation wall-clock budget expired")]
    Deadline,
}

/// Executes one admitted provider tool call and transfers immutable evidence
/// from the adapter board into the incident runtime.
pub async fn execute_model_tool(
    grafana: &GrafanaContext,
    runtime: &mut IncidentRuntime,
    board: &mut super::evidence::EvidenceBoard,
    context_budget: &mut ContextBudget,
    loop_state: &mut ToolLoop,
    call: &ToolCall,
) -> Result<ToolResult, InvestigationError> {
    if let Err(error) = loop_state.admit(call) {
        let class = if matches!(error, ToolLoopError::TurnLimit) {
            ToolResultClass::Exhausted
        } else {
            ToolResultClass::Denied
        };
        let result = ToolResult {
            call_id: call.call_id.clone(),
            class,
            evidence_id: None,
            detail: error.to_string(),
        };
        loop_state.record_result(result.clone());
        return Ok(result);
    }
    if runtime.reserve_evidence_query().is_err() {
        let result = ToolResult {
            call_id: call.call_id.clone(),
            class: ToolResultClass::Exhausted,
            evidence_id: None,
            detail: "incident evidence-query budget exhausted".to_owned(),
        };
        loop_state.record_result(result.clone());
        return Ok(result);
    }
    let Some(remaining) = runtime.remaining() else {
        let result = ToolResult {
            call_id: call.call_id.clone(),
            class: ToolResultClass::Exhausted,
            evidence_id: None,
            detail: "incident deadline exhausted before context query".to_owned(),
        };
        loop_state.record_result(result.clone());
        return Ok(result);
    };
    let mut result =
        tokio::time::timeout(remaining, grafana.execute_tool(board, context_budget, call))
            .await
            .unwrap_or_else(|_| ToolResult {
                call_id: call.call_id.clone(),
                class: ToolResultClass::Exhausted,
                evidence_id: None,
                detail: "incident deadline exhausted during context query".to_owned(),
            });
    if result.class == ToolResultClass::Succeeded {
        if let Some(evidence_id) = &result.evidence_id {
            if let Some(record) = board
                .records()
                .iter()
                .find(|record| &record.evidence_id == evidence_id)
            {
                let runtime_evidence_id = runtime.commit_evidence(
                    record.source,
                    record.query.clone(),
                    record.payload.clone(),
                    runtime.elapsed_ms(),
                )?;
                result.evidence_id = Some(runtime_evidence_id);
                result.detail = redact_text(&String::from_utf8_lossy(&record.payload));
                if result.detail.len() > 16_384 {
                    let mut limit = 16_384;
                    while !result.detail.is_char_boundary(limit) {
                        limit -= 1;
                    }
                    result.detail.truncate(limit);
                }
            }
        }
    }
    loop_state.record_result(result.clone());
    Ok(result)
}

/// Inputs for one live-provider investigation.
pub struct LiveInvestigationInput<'a> {
    /// Read-only Grafana context.
    pub grafana: &'a GrafanaContext,
    /// Single-writer durable journal.
    pub journal: &'a mut JournalStore,
    /// Normalized incident signal.
    pub signal: &'a IncidentSignal,
    /// Stable run identity for this investigation attempt.
    pub run_id: &'a str,
    /// Fresh per-incident reasoning runtime.
    pub runtime: &'a mut IncidentRuntime,
    /// Configured live providers.
    pub providers: LiveProviders<'a>,
    /// Worst-case reservation for each provider restart.
    pub reservation: Reservation,
    /// Explicit read-only evidence queries.
    pub queries: InvestigationQueries,
    /// Monotonic incident timestamp.
    pub start_at_ms: u64,
}

/// Runs one live-provider investigation using an existing single-writer journal.
pub async fn investigate_live(
    input: LiveInvestigationInput<'_>,
) -> Result<InvestigationResult, InvestigationError> {
    let LiveInvestigationInput {
        grafana,
        journal,
        signal,
        run_id,
        runtime,
        providers,
        reservation,
        queries,
        start_at_ms,
    } = input;
    let context = JournalContext {
        incident_id: signal.incident_id.clone(),
        run_id: run_id.to_owned(),
    };
    journal.append_scoped(
        JournalEvent::PhaseStarted {
            phase: Phase::Investigation,
            at_ms: start_at_ms,
        },
        Some(&context),
    )?;
    let mut collected = super::evidence::EvidenceBoard::default();
    let mut context_budget = ContextBudget::new(runtime.max_evidence_queries() as usize);
    let requests = [
        ReadOnlyRequest::Logs(queries.logs),
        ReadOnlyRequest::Metrics(queries.metrics),
    ];
    for request in requests {
        let remaining = runtime.remaining().ok_or(InvestigationError::Deadline)?;
        let result = timeout(remaining, async {
            match request {
                ReadOnlyRequest::Logs(expression) => {
                    grafana
                        .logs_with_budget(&mut collected, &mut context_budget, expression)
                        .await
                }
                ReadOnlyRequest::Metrics(expression) => {
                    grafana
                        .metrics_with_budget(&mut collected, &mut context_budget, expression)
                        .await
                }
            }
        })
        .await
        .map_err(|_| InvestigationError::Deadline)??;
        let record = collected
            .records()
            .iter()
            .find(|record| record.evidence_id == result)
            .expect("context query commits its evidence");
        runtime.reserve_evidence_query()?;
        runtime.commit_evidence(
            record.source,
            record.query.clone(),
            record.payload.clone(),
            runtime.elapsed_ms(),
        )?;
    }
    journal.append_scoped(
        JournalEvent::PhaseFinished {
            phase: Phase::Investigation,
            at_ms: runtime.elapsed_ms(),
        },
        Some(&context),
    )?;
    let prompt = build_prompt(signal, runtime);
    let mut tool_board = super::evidence::EvidenceBoard::default();
    let mut tool_context_budget = ContextBudget::new(runtime.max_evidence_queries() as usize);
    let mut persisted_runtime_entries = 0_usize;
    persist_runtime_events(runtime, journal, &context, &mut persisted_runtime_entries)?;
    let run_result = run_live_with_context(ContextLiveInput {
        runtime,
        providers,
        grafana,
        board: &mut tool_board,
        context_budget: &mut tool_context_budget,
        prompt: &prompt,
        alert_name: &signal.alert_name,
        reservation,
        start_at_ms,
        journal,
        journal_context: &context,
        persisted_runtime_entries: &mut persisted_runtime_entries,
    })
    .await?;
    persist_runtime_events(runtime, journal, &context, &mut persisted_runtime_entries)?;
    let status = run_result;
    Ok(InvestigationResult {
        incident_id: signal.incident_id.clone(),
        evidence_ids: runtime
            .evidence()
            .records()
            .iter()
            .map(|record| record.evidence_id.clone())
            .collect(),
        status,
        report: runtime.last_report().cloned().unwrap_or_else(|| {
            super::contracts::DiagnosticReport {
                summary: "No provider produced an accepted diagnosis within the incident budget."
                    .to_owned(),
                evidence: Vec::new(),
            }
        }),
    })
}

/// Runs provider turns and resumes the same provider after each context result.
struct ContextLiveInput<'a> {
    runtime: &'a mut IncidentRuntime,
    providers: LiveProviders<'a>,
    grafana: &'a GrafanaContext,
    board: &'a mut super::evidence::EvidenceBoard,
    context_budget: &'a mut ContextBudget,
    prompt: &'a str,
    alert_name: &'a str,
    reservation: Reservation,
    start_at_ms: u64,
    journal: &'a mut JournalStore,
    journal_context: &'a JournalContext,
    persisted_runtime_entries: &'a mut usize,
}

fn persist_runtime_events(
    runtime: &IncidentRuntime,
    journal: &mut JournalStore,
    context: &JournalContext,
    persisted_runtime_entries: &mut usize,
) -> Result<(), InvestigationError> {
    let entries = runtime.journal().entries();
    if *persisted_runtime_entries >= entries.len() {
        return Ok(());
    }
    let events = entries[*persisted_runtime_entries..]
        .iter()
        .map(|entry| entry.event.clone())
        .collect::<Vec<_>>();
    journal.append_with_outbox_scoped(&events, None, Some(context))?;
    *persisted_runtime_entries = entries.len();
    Ok(())
}

async fn run_live_with_context(
    input: ContextLiveInput<'_>,
) -> Result<super::coordinator::RunStatus, InvestigationError> {
    let ContextLiveInput {
        runtime,
        providers,
        grafana,
        board,
        context_budget,
        prompt,
        alert_name,
        reservation,
        start_at_ms,
        journal,
        journal_context,
        persisted_runtime_entries,
    } = input;
    let mut at_ms = start_at_ms;
    loop {
        let provider = match runtime.admit_provider(reservation, at_ms)? {
            Some(provider) => provider,
            None => return Ok(super::coordinator::RunStatus::Exhausted),
        };
        let mut loop_state = ToolLoop::new(
            super::tools::ToolTurnPolicy::new(runtime.max_tool_turns()).map_err(|_| {
                RuntimeError::Coordination(super::coordinator::CoordinatorError::InvalidOrder(
                    "tool turn limit",
                ))
            })?,
        );
        let mut turn = match runtime.remaining() {
            Some(remaining) => timeout(
                remaining,
                provider_turn(&providers, provider, prompt, &[], alert_name),
            )
            .await
            .unwrap_or(Err(
                super::coordinator::FailureClass::TemporarilyUnavailable,
            )),
            None => Err(super::coordinator::FailureClass::TemporarilyUnavailable),
        };
        loop {
            let started = std::time::Instant::now();
            match turn {
                Ok(super::live::LiveTurn::Final { report, tokens }) => {
                    let facts = super::coordinator::AttemptFacts {
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        tokens,
                        evidence_queries: loop_state.results().len() as u32,
                        ..Default::default()
                    };
                    let evidence_ids = runtime
                        .evidence()
                        .records()
                        .iter()
                        .map(|record| record.evidence_id.clone())
                        .collect::<BTreeSet<_>>();
                    if report.validate_against(&evidence_ids).is_ok() {
                        return Ok(
                            runtime.succeed_provider_with_report(provider, report, facts, at_ms)?
                        );
                    }
                    runtime.fail_provider(
                        provider,
                        super::coordinator::FailureClass::MalformedResponse,
                        facts,
                        at_ms,
                    )?;
                    break;
                }
                Ok(super::live::LiveTurn::ToolCalls { calls, .. }) => {
                    if calls.is_empty() {
                        let facts = super::coordinator::AttemptFacts {
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            evidence_queries: loop_state.results().len() as u32,
                            ..Default::default()
                        };
                        runtime.fail_provider(
                            provider,
                            super::coordinator::FailureClass::MalformedResponse,
                            facts,
                            at_ms,
                        )?;
                        break;
                    }
                    for call in calls {
                        let evidence_queries_before = runtime.budget_totals().2;
                        let tool_started = std::time::Instant::now();
                        let result = execute_model_tool(
                            grafana,
                            runtime,
                            board,
                            context_budget,
                            &mut loop_state,
                            &call,
                        )
                        .await
                        .map_err(|error| {
                            RuntimeError::Evidence(match error {
                                InvestigationError::Context(ContextError::Evidence(e)) => e,
                                _ => crate::reasoning::evidence::EvidenceError::EmptyPayload,
                            })
                        })?;
                        let evidence_queries_after = runtime.budget_totals().2;
                        runtime.record_tool_context(super::journal::ToolContextFacts {
                            provider,
                            tool: tool_name(&call.tool).to_owned(),
                            query_digest: super::journal::query_digest(&call.query),
                            result_class: result.class,
                            elapsed_ms: tool_started.elapsed().as_millis() as u64,
                            output_bytes: result.detail.len() as u64,
                            evidence_queries_before,
                            evidence_queries_after,
                            at_ms: runtime.elapsed_ms(),
                        });
                        persist_runtime_events(
                            runtime,
                            journal,
                            journal_context,
                            persisted_runtime_entries,
                        )?;
                    }
                    turn = match runtime.remaining() {
                        Some(remaining) => timeout(
                            remaining,
                            provider_turn(
                                &providers,
                                provider,
                                prompt,
                                loop_state.results(),
                                alert_name,
                            ),
                        )
                        .await
                        .unwrap_or(Err(
                            super::coordinator::FailureClass::TemporarilyUnavailable,
                        )),
                        None => Err(super::coordinator::FailureClass::TemporarilyUnavailable),
                    };
                    continue;
                }
                Err(failure) => {
                    let facts = super::coordinator::AttemptFacts {
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        evidence_queries: loop_state.results().len() as u32,
                        ..Default::default()
                    };
                    runtime.fail_provider(provider, failure, facts, at_ms)?;
                    break;
                }
            }
        }
        at_ms = runtime.elapsed_ms();
    }
}

fn tool_name(tool: &super::tools::ContextTool) -> &'static str {
    match tool {
        super::tools::ContextTool::QueryLogs => "query_logs",
        super::tools::ContextTool::QueryMetrics => "query_metrics",
    }
}

async fn provider_turn<'a>(
    providers: &LiveProviders<'a>,
    provider: ProviderKind,
    prompt: &'a str,
    results: &'a [ToolResult],
    alert_name: &'a str,
) -> Result<super::live::LiveTurn, super::coordinator::FailureClass> {
    match provider {
        ProviderKind::OpenAi => match providers.openai {
            Some(provider) => provider.complete_with_results(prompt, results).await,
            None => Err(super::coordinator::FailureClass::TemporarilyUnavailable),
        },
        ProviderKind::Gemini => match providers.gemini {
            Some(provider) => provider.complete_with_results(prompt, results).await,
            None => Err(super::coordinator::FailureClass::TemporarilyUnavailable),
        },
        ProviderKind::DeepSeek => match providers.deepseek {
            Some(provider) => provider.complete_with_results(prompt, results).await,
            None => Err(super::coordinator::FailureClass::TemporarilyUnavailable),
        },
        ProviderKind::Deterministic => Ok(super::live::LiveTurn::Final {
            report: super::baseline::build_report(alert_name, &BTreeSet::new()),
            tokens: None,
        }),
    }
}

fn build_prompt(signal: &IncidentSignal, runtime: &IncidentRuntime) -> String {
    let mut prompt = format!(
        "Diagnose incident {} (alert {}). Return strict JSON with summary and evidence citations.\nTools available: query_logs(query), query_metrics(query). These are read-only, bounded, and datasource-owned.\nRemaining configured evidence-query ceiling: {}. Do not execute commands, write Grafana state, or request credentials.\n",
        signal.incident_id,
        signal.alert_name,
        runtime
            .max_evidence_queries()
            .saturating_sub(runtime.budget_totals().2)
    );
    for record in runtime.evidence().records() {
        let payload = redact_text(&String::from_utf8_lossy(&record.payload));
        prompt.push_str(&format!(
            "Evidence {} ({:?}, query={}): {}\n",
            record.evidence_id, record.source, record.query, payload
        ));
        if prompt.len() >= 32_768 {
            let mut limit = 32_768;
            while !prompt.is_char_boundary(limit) {
                limit -= 1;
            }
            prompt.truncate(limit);
            break;
        }
    }
    prompt
}

/// Redacts common credential-bearing fields before evidence crosses an LLM or
/// operator-notification boundary.
pub fn redact_text(input: &str) -> String {
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(input) {
        redact_json(&mut value);
        return serde_json::to_string(&value).unwrap_or_else(|_| "[REDACTED EVIDENCE]".to_owned());
    }
    let mut redact_next = false;
    input
        .split_whitespace()
        .map(|word| {
            if redact_next {
                redact_next = false;
                return "[REDACTED]";
            }
            let lower = word.to_ascii_lowercase();
            if lower == "bearer" {
                redact_next = true;
                return word;
            }
            if ["token=", "password=", "secret=", "api_key=", "apikey="]
                .iter()
                .any(|prefix| lower.starts_with(prefix))
            {
                word.split_once('=')
                    .map_or("Bearer [REDACTED]", |(prefix, _)| {
                        let _ = prefix;
                        "[REDACTED]"
                    })
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                let sensitive = key.to_ascii_lowercase().contains("token")
                    || key.to_ascii_lowercase().contains("secret")
                    || key.to_ascii_lowercase().contains("password")
                    || key.to_ascii_lowercase().contains("api_key")
                    || key.eq_ignore_ascii_case("authorization");
                if sensitive {
                    *child = serde_json::Value::String("[REDACTED]".to_owned());
                } else {
                    redact_json(child);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json),
        _ => {}
    }
}

/// Runs one investigation and persists its redacted lifecycle facts.
#[derive(Debug)]
pub struct ShadowInvestigator {
    grafana: GrafanaContext,
    journal: JournalStore,
}

impl ShadowInvestigator {
    /// Creates a shadow investigator from read-only Grafana and journal bounds.
    pub fn new(grafana: GrafanaContext, journal: JournalStore) -> Self {
        Self { grafana, journal }
    }

    /// Collects evidence, runs finite provider fallback, and persists facts.
    pub async fn investigate(
        &mut self,
        signal: &IncidentSignal,
        runtime: &mut IncidentRuntime,
        providers: &mut [RecordedProvider],
        reservation: Reservation,
        queries: InvestigationQueries,
        start_at_ms: u64,
    ) -> Result<InvestigationResult, InvestigationError> {
        self.journal.append(JournalEvent::IncidentOpened {
            incident_id: signal.incident_id.clone(),
            alert_name: signal.alert_name.clone(),
        })?;
        self.journal.append(JournalEvent::PhaseStarted {
            phase: Phase::Investigation,
            at_ms: start_at_ms,
        })?;

        let mut collected = super::evidence::EvidenceBoard::default();
        self.grafana.logs(&mut collected, queries.logs).await?;
        self.grafana
            .metrics(&mut collected, queries.metrics)
            .await?;
        for record in collected.records() {
            runtime.commit_evidence(
                record.source,
                record.query.clone(),
                record.payload.clone(),
                start_at_ms,
            )?;
        }
        self.journal.append(JournalEvent::PhaseFinished {
            phase: Phase::Investigation,
            at_ms: start_at_ms,
        })?;
        let status = runtime.run_recorded(providers, reservation, start_at_ms)?;
        for entry in runtime.journal().entries() {
            self.journal.append(entry.event.clone())?;
        }

        Ok(InvestigationResult {
            incident_id: signal.incident_id.clone(),
            evidence_ids: runtime
                .evidence()
                .records()
                .iter()
                .map(|record| record.evidence_id.clone())
                .collect(),
            status,
            report: runtime.last_report().cloned().unwrap_or_else(|| {
                super::contracts::DiagnosticReport {
                    summary:
                        "No provider produced an accepted diagnosis within the incident budget."
                            .to_owned(),
                    evidence: Vec::new(),
                }
            }),
        })
    }

    /// Exposes the durable journal for replay and reporting.
    pub fn journal(&self) -> &JournalStore {
        &self.journal
    }

    /// Builds conservative queries from an explicitly selected service name.
    pub fn queries_for_service(service: &str) -> Option<InvestigationQueries> {
        if service.is_empty()
            || !service.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
            })
        {
            return None;
        }
        Some(InvestigationQueries {
            logs: format!(r#"{{service="{service}"}} |= "error""#),
            metrics: format!(r#"up{{service="{service}"}}"#),
        })
    }

    /// Returns the provider that produced a successful report, if any.
    pub fn terminal_provider(result: &InvestigationResult) -> Option<ProviderKind> {
        match result.status {
            super::coordinator::RunStatus::Succeeded(provider) => Some(provider),
            super::coordinator::RunStatus::Exhausted => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::persist_runtime_events;
    use crate::adapters::grafana::context::{ContextBudget, GrafanaContext, GrafanaContextConfig};
    use crate::reasoning::{
        budget::Reservation,
        coordinator::ReasoningConfig,
        journal::{JournalContext, ToolContextFacts, query_digest},
        live::{LiveCompletion, LiveProvider, LiveProviders, LiveTurn},
        router::ProviderKind,
        runtime::IncidentRuntime,
        storage::JournalStore,
        tools::{ContextTool, ToolCall, ToolResultClass},
    };
    use std::{sync::Mutex, time::Duration};

    #[test]
    fn tool_checkpoint_survives_store_reopen() {
        // Given a runtime with committed evidence and a completed tool fact.
        let path = std::env::temp_dir().join(format!(
            "ai-sre-tool-checkpoint-{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let context = JournalContext {
            incident_id: "incident-checkpoint".to_owned(),
            run_id: "run-checkpoint".to_owned(),
        };
        let mut runtime = IncidentRuntime::new(ReasoningConfig::default()).expect("runtime");
        runtime
            .commit_evidence(
                crate::reasoning::evidence::EvidenceSource::GrafanaLogs,
                "{app=\"api\"}",
                b"bounded output".to_vec(),
                1,
            )
            .expect("evidence");
        runtime.record_tool_context(ToolContextFacts {
            provider: ProviderKind::OpenAi,
            tool: "query_logs".to_owned(),
            query_digest: query_digest("{app=\"api\"}"),
            result_class: ToolResultClass::Succeeded,
            elapsed_ms: 4,
            output_bytes: 14,
            evidence_queries_before: 0,
            evidence_queries_after: 1,
            at_ms: 4,
        });
        let mut store = JournalStore::open(&path).expect("store");
        let mut persisted = 0;

        // When the causal runtime events are flushed as one durable checkpoint.
        persist_runtime_events(&runtime, &mut store, &context, &mut persisted).expect("checkpoint");
        drop(store);

        // Then replay preserves evidence and tool accounting.
        let reopened = JournalStore::open(&path).expect("reopen");
        let projection = reopened.efficiency_projection(&context);
        assert_eq!(projection.tool_calls, 1);
        assert_eq!(projection.successful_tool_calls, 1);
        assert_eq!(reopened.journal().entries().len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    struct ScriptedProvider {
        turns: Mutex<u8>,
    }

    impl LiveProvider for ScriptedProvider {
        fn complete<'a>(&'a self, _prompt: &'a str) -> LiveCompletion<'a> {
            Box::pin(async { Err(crate::reasoning::coordinator::FailureClass::MalformedResponse) })
        }

        fn complete_with_results<'a>(
            &'a self,
            _prompt: &'a str,
            results: &'a [crate::reasoning::tools::ToolResult],
        ) -> LiveCompletion<'a> {
            let is_first = {
                let mut turns = self.turns.lock().expect("turn lock");
                let first = *turns == 0;
                *turns += 1;
                first
            };
            Box::pin(async move {
                if is_first {
                    Ok(LiveTurn::ToolCalls {
                        calls: vec![ToolCall {
                            call_id: "call-1".to_owned(),
                            tool: ContextTool::QueryLogs,
                            query: "{app=\"api\"}".to_owned(),
                        }],
                        tokens: None,
                    })
                } else {
                    let evidence_id = results
                        .first()
                        .and_then(|result| result.evidence_id.clone())
                        .expect("tool evidence");
                    Ok(LiveTurn::Final {
                        report: crate::reasoning::contracts::DiagnosticReport {
                            summary: "The API evidence is available.".to_owned(),
                            evidence: vec![crate::reasoning::contracts::EvidenceRef {
                                evidence_id,
                            }],
                        },
                        tokens: None,
                    })
                }
            })
        }
    }

    #[tokio::test]
    async fn live_tool_call_is_checkpointed_before_provider_resumption() {
        // Given a scripted provider and read-only GCX adapter.
        let path = std::env::temp_dir().join(format!(
            "ai-sre-live-tool-checkpoint-{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let context = JournalContext {
            incident_id: "incident-live".to_owned(),
            run_id: "run-live".to_owned(),
        };
        let mut journal = JournalStore::open(&path).expect("journal");
        let grafana = GrafanaContext::new(
            crate::adapters::grafana::gcx::GcxRunner::new(
                "/bin/echo",
                Duration::from_secs(1),
                4096,
            ),
            GrafanaContextConfig {
                logs_datasource: "loki".to_owned(),
                metrics_datasource: "prometheus".to_owned(),
            },
        );
        let provider = ScriptedProvider {
            turns: Mutex::new(0),
        };
        let mut runtime = IncidentRuntime::new(ReasoningConfig::default()).expect("runtime");
        let mut board = super::super::evidence::EvidenceBoard::default();
        let mut budget = ContextBudget::new(3);
        let mut persisted = 0;

        // When one tool turn is executed and the provider resumes with its result.
        let status = super::run_live_with_context(super::ContextLiveInput {
            runtime: &mut runtime,
            providers: LiveProviders {
                openai: Some(&provider),
                gemini: None,
                deepseek: None,
            },
            grafana: &grafana,
            board: &mut board,
            context_budget: &mut budget,
            prompt: "diagnose",
            alert_name: "ApiDown",
            reservation: Reservation {
                provider_calls: 1,
                tokens: 0,
                evidence_queries: 0,
                cost_micro_usd: 0,
            },
            start_at_ms: 0,
            journal: &mut journal,
            journal_context: &context,
            persisted_runtime_entries: &mut persisted,
        })
        .await
        .expect("live loop");
        assert_eq!(
            status,
            crate::reasoning::coordinator::RunStatus::Succeeded(ProviderKind::OpenAi)
        );
        drop(journal);

        // Then the reopened journal contains the tool fact before any later replay.
        let reopened = JournalStore::open(&path).expect("reopen");
        assert_eq!(reopened.efficiency_projection(&context).tool_calls, 1);
        assert_eq!(
            reopened
                .efficiency_projection(&context)
                .successful_tool_calls,
            1
        );
        let _ = std::fs::remove_file(&path);
    }
}
