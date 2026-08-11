//! Provider-neutral diagnostic report acceptance and rejection scenarios.

use std::collections::BTreeSet;

use ai_sre::adapters::llm::{
    ApiKey, ProviderFailure, deepseek::DeepSeekClient, gemini::GeminiClient,
};
use ai_sre::reasoning::budget::{BudgetConfig, BudgetError, BudgetState, Reservation};
use ai_sre::reasoning::contracts::{ContractError, DiagnosticReport, EvidenceRef};
use ai_sre::reasoning::coordinator::{
    AttemptFacts, AttemptOutcome, AttemptRecord, CoordinatorError, FailureClass, ReasoningConfig,
    ReasoningRun, RunStatus,
};
use ai_sre::reasoning::evidence::{EvidenceBoard, EvidenceError, EvidenceSource};
use ai_sre::reasoning::journal::{IncidentJournal, JournalEvent, Phase, attempt_event};
use ai_sre::reasoning::recorded::{RecordedAttempt, RecordedOutcome, RecordedProvider};
use ai_sre::reasoning::router::{ProviderKind, ProviderOrder, next_provider, next_provider_in};
use ai_sre::reasoning::runtime::IncidentRuntime;
use ai_sre::{
    bootstrap,
    config::{AppConfig, ConfigError},
};

#[test]
fn api_keys_are_redacted_and_provider_clients_normalize_common_reports() {
    // Given provider envelopes containing the same strict diagnostic JSON.
    let report = r#"{"summary":"The API is returning elevated 5xx responses.","evidence":[{"evidence_id":"metrics-001"}]}"#;
    let gemini = format!(
        r#"{{"candidates":[{{"content":{{"parts":[{{"text":{}}}]}}}}]}}"#,
        serde_json::to_string(report).unwrap()
    );
    let deepseek = format!(
        r#"{{"choices":[{{"message":{{"content":{}}}}}]}}"#,
        serde_json::to_string(report).unwrap()
    );

    // When both adapters normalize their vendor envelopes.
    let gemini_result = GeminiClient::new("gemini-secret").normalize_report(&gemini);
    let deepseek_result = DeepSeekClient::new("deepseek-secret").normalize_report(&deepseek);

    // Then both return the same provider-neutral artifact and secrets stay redacted.
    assert_eq!(gemini_result, deepseek_result);
    assert!(!format!("{:?}", ApiKey::new("gemini-secret")).contains("gemini-secret"));
}

#[test]
fn malformed_gemini_and_deepseek_envelopes_fail_without_vendor_details() {
    // Given envelopes with no usable model message.
    let gemini = r#"{"candidates":[]}"#;
    let deepseek = r#"{"choices":[]}"#;

    // When each adapter attempts normalization.
    let gemini_result = GeminiClient::new("gemini-secret").normalize_report(gemini);
    let deepseek_result = DeepSeekClient::new("deepseek-secret").normalize_report(deepseek);

    // Then both expose only the shared safe failure classification.
    assert_eq!(gemini_result, Err(ProviderFailure::MalformedResponse));
    assert_eq!(deepseek_result, Err(ProviderFailure::MalformedResponse));
}

#[test]
fn failover_router_advances_without_repeating_a_provider() {
    // Given a run that already attempted OpenAI and Gemini.
    let attempted = BTreeSet::from([ProviderKind::OpenAi, ProviderKind::Gemini]);

    // When the pure router chooses the next provider.
    let next = next_provider(&attempted);

    // Then it selects DeepSeek, preserving the fixed complete-run order.
    assert_eq!(next, Some(ProviderKind::DeepSeek));
}

#[test]
fn failover_router_ends_with_deterministic_baseline() {
    // Given a run where all model providers have failed.
    let attempted = BTreeSet::from([
        ProviderKind::OpenAi,
        ProviderKind::Gemini,
        ProviderKind::DeepSeek,
    ]);

    // When the router is asked for the final fallback.
    let next = next_provider(&attempted);

    // Then it returns the deterministic baseline.
    assert_eq!(next, Some(ProviderKind::Deterministic));
}

#[test]
fn diagnostic_report_accepts_only_citations_from_the_evidence_board() {
    // Given an evidence board and a report citing one committed evidence item.
    let available = BTreeSet::from(["logs-001".to_owned(), "metrics-001".to_owned()]);
    let report = DiagnosticReport {
        summary: "The API is returning elevated 5xx responses.".to_owned(),
        evidence: vec![EvidenceRef {
            evidence_id: "metrics-001".to_owned(),
        }],
    };

    // When the report is validated against the available evidence identifiers.
    let result = report.validate_against(&available);

    // Then validation accepts the report.
    assert_eq!(result, Ok(()));
}

#[test]
fn diagnostic_report_rejects_a_citation_not_on_the_evidence_board() {
    // Given an evidence board that does not contain the report's citation.
    let available = BTreeSet::from(["logs-001".to_owned()]);
    let report = DiagnosticReport {
        summary: "The API is returning elevated 5xx responses.".to_owned(),
        evidence: vec![EvidenceRef {
            evidence_id: "invented-001".to_owned(),
        }],
    };

    // When the report is validated against that board.
    let result = report.validate_against(&available);

    // Then validation rejects the unknown evidence identifier.
    assert_eq!(
        result,
        Err(ContractError::UnknownEvidence("invented-001".to_owned()))
    );
}

#[test]
fn diagnostic_report_rejects_an_empty_summary() {
    // Given a report whose summary contains only whitespace.
    let report = DiagnosticReport {
        summary: "  ".to_owned(),
        evidence: Vec::new(),
    };

    // When the report is validated without any evidence.
    let result = report.validate_against(&BTreeSet::new());

    // Then validation reports the empty-summary failure first.
    assert_eq!(result, Err(ContractError::EmptySummary));
}

#[test]
fn diagnostic_report_rejects_missing_evidence() {
    // Given a non-empty summary with no evidence citations.
    let report = DiagnosticReport {
        summary: "The API is returning elevated 5xx responses.".to_owned(),
        evidence: Vec::new(),
    };

    // When the report is validated against an empty evidence board.
    let result = report.validate_against(&BTreeSet::new());

    // Then validation rejects the report for missing evidence.
    assert_eq!(result, Err(ContractError::MissingEvidence));
}

#[test]
fn provider_json_rejects_unknown_fields() {
    // Given provider JSON containing an undeclared field inside an evidence item.
    let json = r#"{
        "summary": "The API is returning elevated 5xx responses.",
        "evidence": [{"evidence_id": "metrics-001", "claim": "invented"}]
    }"#;

    // When the JSON is deserialized into the provider-neutral report type.
    let result = serde_json::from_str::<DiagnosticReport>(json);

    // Then strict deserialization rejects the unknown field.
    assert!(result.is_err());
}

#[test]
fn provider_json_is_normalized_through_the_core_contract() {
    // Given valid provider JSON and an evidence board containing its citation.
    let json = r#"{
        "summary": "The API is returning elevated 5xx responses.",
        "evidence": [{"evidence_id": "metrics-001"}]
    }"#;
    let available = BTreeSet::from(["metrics-001".to_owned()]);

    // When the response is normalized and validated by the core contract.
    let report = DiagnosticReport::from_provider_json(json)
        .expect("provider response should deserialize")
        .validate_against(&available);

    // Then the provider-neutral report is accepted.
    assert_eq!(report, Ok(()));
}

#[test]
fn provider_json_rejects_malformed_output_without_exposing_provider_details() {
    // Given provider JSON with the wrong type for the summary field.
    let json = r#"{"summary": 42, "evidence": []}"#;

    // When the response is normalized through the public contract boundary.
    let result = DiagnosticReport::from_provider_json(json);

    // Then parsing returns a safe classified error without provider payload details.
    assert_eq!(result, Err(ContractError::MalformedProviderResponse));
}

#[test]
fn provider_order_is_configurable_without_allowing_duplicates() {
    // Given an operator-selected order that starts with Gemini.
    let order = ProviderOrder {
        providers: vec![ProviderKind::Gemini, ProviderKind::Deterministic],
    };
    let attempted = BTreeSet::from([ProviderKind::Gemini]);

    // When the configured order is validated and queried.
    let next = next_provider_in(&attempted, &order);

    // Then the next provider follows configuration rather than a hard-coded list.
    assert_eq!(order.validate(), Ok(()));
    assert_eq!(next, Some(ProviderKind::Deterministic));
}

#[test]
fn budget_reservation_is_atomic_and_rejects_overcommitment() {
    // Given a deliberately small incident budget.
    let config = BudgetConfig {
        max_provider_calls: 2,
        max_tokens: 100,
        max_evidence_queries: 4,
        max_wall_time_secs: 30,
        max_cost_micro_usd: Some(500),
    };
    let mut state = BudgetState::new(config);
    let first = Reservation {
        provider_calls: 1,
        tokens: 60,
        evidence_queries: 2,
        cost_micro_usd: 300,
    };

    // When a second reservation would exceed the token ceiling.
    assert_eq!(state.reserve(first), Ok(()));
    let before = state.totals();
    let rejected = state.reserve(Reservation {
        provider_calls: 1,
        tokens: 50,
        evidence_queries: 1,
        cost_micro_usd: 100,
    });

    // Then rejection leaves every accounting dimension unchanged.
    assert_eq!(rejected, Err(BudgetError::Tokens));
    assert_eq!(state.totals(), before);
}

#[test]
fn coordinator_reserves_before_each_restart_and_ends_in_baseline() {
    // Given a run configured for two provider attempts followed by baseline.
    let config = ReasoningConfig {
        provider_order: ProviderOrder {
            providers: vec![ProviderKind::OpenAi, ProviderKind::Deterministic],
        },
        budget: BudgetConfig {
            max_provider_calls: 2,
            ..BudgetConfig::default()
        },
    };
    let mut run = ReasoningRun::new(config).expect("valid run configuration");
    let reservation = Reservation {
        provider_calls: 1,
        tokens: 100,
        evidence_queries: 1,
        cost_micro_usd: 0,
    };

    // When OpenAI fails, the coordinator restarts from the same run-scoped board.
    assert_eq!(run.admit(reservation), Ok(Some(ProviderKind::OpenAi)));
    run.fail(ProviderKind::OpenAi).expect("active provider");

    // Then the configured deterministic provider is admitted and can terminate the run.
    assert_eq!(
        run.admit(reservation),
        Ok(Some(ProviderKind::Deterministic))
    );
    assert_eq!(
        run.succeed(ProviderKind::Deterministic),
        Ok(RunStatus::Succeeded(ProviderKind::Deterministic))
    );
    assert_eq!(
        run.status(),
        Some(RunStatus::Succeeded(ProviderKind::Deterministic))
    );
}

#[test]
fn coordinator_rejects_wrong_completion_without_changing_active_run() {
    // Given an active OpenAI run.
    let mut run = ReasoningRun::new(ReasoningConfig::default()).expect("default config");
    let reservation = Reservation {
        provider_calls: 1,
        tokens: 100,
        evidence_queries: 1,
        cost_micro_usd: 0,
    };
    assert_eq!(run.admit(reservation), Ok(Some(ProviderKind::OpenAi)));

    // When another provider falsely reports completion.
    let result = run.succeed(ProviderKind::Gemini);

    // Then the coordinator rejects it and keeps the active run intact.
    assert_eq!(result, Err(CoordinatorError::WrongProvider));
    assert_eq!(
        run.succeed(ProviderKind::OpenAi),
        Ok(RunStatus::Succeeded(ProviderKind::OpenAi))
    );
}

#[test]
fn coordinator_preserves_unknown_cost_in_attempt_facts() {
    // Given an admitted OpenAI attempt whose OAuth cost cannot be priced.
    let mut run = ReasoningRun::new(ReasoningConfig::default()).expect("default config");
    let reservation = Reservation {
        provider_calls: 1,
        tokens: 100,
        evidence_queries: 2,
        cost_micro_usd: 0,
    };
    assert_eq!(run.admit(reservation), Ok(Some(ProviderKind::OpenAi)));

    // When the attempt fails after measured work with no trustworthy price.
    run.fail_with(
        ProviderKind::OpenAi,
        FailureClass::TemporarilyUnavailable,
        AttemptFacts {
            elapsed_ms: 842,
            tokens: Some(100),
            evidence_queries: 2,
            cost_micro_usd: None,
        },
    )
    .expect("active provider");

    // Then the journal facts retain the unknown cost instead of recording zero.
    assert_eq!(run.attempts().len(), 1);
    assert_eq!(
        run.attempts()[0].outcome,
        AttemptOutcome::Failed(FailureClass::TemporarilyUnavailable)
    );
    assert_eq!(run.attempts()[0].facts.cost_micro_usd, None);
}

#[test]
fn journal_replay_derives_efficiency_without_turning_unknown_cost_into_zero() {
    // Given raw phase boundaries and two provider attempts.
    let mut journal = IncidentJournal::default();
    journal.append(JournalEvent::PhaseStarted {
        phase: Phase::Reasoning,
        at_ms: 100,
    });
    journal.append(attempt_event(AttemptRecord {
        provider: ProviderKind::OpenAi,
        outcome: AttemptOutcome::Failed(FailureClass::TemporarilyUnavailable),
        facts: AttemptFacts {
            elapsed_ms: 800,
            tokens: Some(120),
            evidence_queries: 3,
            cost_micro_usd: None,
        },
    }));
    journal.append(attempt_event(AttemptRecord {
        provider: ProviderKind::Gemini,
        outcome: AttemptOutcome::Succeeded,
        facts: AttemptFacts {
            elapsed_ms: 500,
            tokens: Some(80),
            evidence_queries: 2,
            cost_micro_usd: Some(2400),
        },
    }));
    journal.append(JournalEvent::PhaseFinished {
        phase: Phase::Reasoning,
        at_ms: 1600,
    });
    journal.append(JournalEvent::Terminal {
        provider: Some(ProviderKind::Gemini),
    });

    // When the immutable entries are projected into efficiency metrics.
    let projection = journal.project();

    // Then replay preserves provider time, usage, outcome, and unknown cost.
    assert_eq!(projection.provider_attempts, 2);
    assert_eq!(projection.provider_time_ms, 1300);
    assert_eq!(projection.tokens, 200);
    assert_eq!(projection.evidence_queries, 5);
    assert_eq!(projection.known_cost_micro_usd, 2400);
    assert_eq!(projection.unknown_cost_attempts, 1);
    assert_eq!(projection.terminal_provider, Some(ProviderKind::Gemini));
    assert_eq!(journal.entries()[0].sequence, 0);
    assert_eq!(journal.entries()[4].sequence, 4);
}

#[test]
fn evidence_board_assigns_stable_ids_and_rejects_empty_tool_output() {
    // Given an empty run-scoped evidence board.
    let mut board = EvidenceBoard::default();

    // When the first tool result is committed and an empty result is rejected.
    let first = board.commit(
        EvidenceSource::GrafanaMetrics,
        "up",
        br#"{"data":[]}"#.to_vec(),
    );
    let empty = board.commit(EvidenceSource::GrafanaLogs, "{app=\"api\"}", Vec::new());

    // Then the ID is deterministic and the board remains unchanged by rejection.
    assert_eq!(first, Ok("evidence-0001".to_owned()));
    assert_eq!(empty, Err(EvidenceError::EmptyPayload));
    assert_eq!(board.records().len(), 1);
    assert_eq!(board.records()[0].evidence_id, "evidence-0001");
}

#[test]
fn incident_runtime_emits_evidence_attempt_and_terminal_journal_facts() {
    // Given a runtime with a single deterministic provider path.
    let config = ReasoningConfig {
        provider_order: ProviderOrder {
            providers: vec![ProviderKind::Deterministic],
        },
        budget: BudgetConfig::default(),
    };
    let mut runtime = IncidentRuntime::new(config).expect("runtime config");

    // When evidence is committed, the provider succeeds, and the run closes.
    assert_eq!(
        runtime.commit_evidence(
            EvidenceSource::GrafanaLogs,
            "{app=\"api\"}",
            b"log line".to_vec(),
            10,
        ),
        Ok("evidence-0001".to_owned())
    );
    let reservation = Reservation {
        provider_calls: 1,
        tokens: 10,
        evidence_queries: 1,
        cost_micro_usd: 0,
    };
    assert_eq!(
        runtime.admit_provider(reservation, 20),
        Ok(Some(ProviderKind::Deterministic))
    );
    assert_eq!(
        runtime.succeed_provider(
            ProviderKind::Deterministic,
            AttemptFacts {
                elapsed_ms: 15,
                tokens: None,
                evidence_queries: 1,
                cost_micro_usd: None,
            },
            40,
        ),
        Ok(RunStatus::Succeeded(ProviderKind::Deterministic))
    );

    // Then replay sees all boundary facts and preserves unknown cost.
    assert_eq!(runtime.evidence().records().len(), 1);
    assert_eq!(runtime.journal().entries().len(), 5);
    assert_eq!(runtime.journal().project().unknown_cost_attempts, 1);
}

#[test]
fn recorded_provider_replays_fallback_outcomes_without_network_access() {
    // Given a scripted OpenAI failure followed by a successful Gemini result.
    let mut provider = RecordedProvider::new(
        ProviderKind::OpenAi,
        [RecordedAttempt {
            outcome: RecordedOutcome::Failure(FailureClass::TemporarilyUnavailable),
            facts: AttemptFacts {
                elapsed_ms: 50,
                tokens: Some(12),
                evidence_queries: 1,
                cost_micro_usd: None,
            },
        }],
    );

    // When the harness is consumed by the application test.
    let attempt = provider.take_next().expect("recorded attempt");

    // Then the outcome and accounting facts are exactly reproducible.
    assert_eq!(provider.provider(), ProviderKind::OpenAi);
    assert_eq!(
        attempt.outcome,
        RecordedOutcome::Failure(FailureClass::TemporarilyUnavailable)
    );
    assert_eq!(attempt.facts.elapsed_ms, 50);
    assert!(provider.take_next().is_none());
}

#[test]
fn runtime_runs_recorded_fallback_from_openai_to_gemini() {
    // Given one evidence item and a scripted OpenAI failure plus deterministic success.
    let mut runtime = IncidentRuntime::new(ReasoningConfig::default()).expect("runtime config");
    runtime
        .commit_evidence(EvidenceSource::GrafanaMetrics, "up", b"metric".to_vec(), 1)
        .expect("evidence");
    let report =
        r#"{"summary":"The service is healthy.","evidence":[{"evidence_id":"evidence-0001"}]}"#;
    let mut providers = vec![
        RecordedProvider::new(
            ProviderKind::OpenAi,
            [RecordedAttempt {
                outcome: RecordedOutcome::Failure(FailureClass::TemporarilyUnavailable),
                facts: AttemptFacts {
                    elapsed_ms: 10,
                    ..AttemptFacts::default()
                },
            }],
        ),
        RecordedProvider::new(
            ProviderKind::Gemini,
            [RecordedAttempt {
                outcome: RecordedOutcome::Success(report.to_owned()),
                facts: AttemptFacts {
                    elapsed_ms: 20,
                    tokens: Some(40),
                    ..AttemptFacts::default()
                },
            }],
        ),
    ];

    // When the runtime executes the complete scripted run.
    let result = runtime.run_recorded(
        &mut providers,
        Reservation {
            provider_calls: 1,
            tokens: 100,
            evidence_queries: 1,
            cost_micro_usd: 0,
        },
        10,
    );

    // Then the failed OpenAI run is discarded and Gemini is the terminal provider.
    assert_eq!(result, Ok(RunStatus::Succeeded(ProviderKind::Gemini)));
    let projection = runtime.journal().project();
    assert_eq!(projection.provider_attempts, 2);
    assert_eq!(projection.terminal_provider, Some(ProviderKind::Gemini));
}

#[test]
fn runtime_runs_full_three_provider_fallback_and_journals_terminal_deepseek() {
    // Given an evidence board and failures from OpenAI and Gemini before DeepSeek succeeds.
    let mut runtime = IncidentRuntime::new(ReasoningConfig::default()).expect("runtime config");
    runtime
        .commit_evidence(
            EvidenceSource::GrafanaLogs,
            "{app=\"api\"}",
            b"log".to_vec(),
            1,
        )
        .expect("evidence");
    let report = r#"{"summary":"Database latency is elevated.","evidence":[{"evidence_id":"evidence-0001"}]}"#;
    let mut providers = vec![
        RecordedProvider::new(
            ProviderKind::OpenAi,
            [RecordedAttempt {
                outcome: RecordedOutcome::Failure(FailureClass::TemporarilyUnavailable),
                facts: AttemptFacts {
                    elapsed_ms: 10,
                    ..AttemptFacts::default()
                },
            }],
        ),
        RecordedProvider::new(
            ProviderKind::Gemini,
            [RecordedAttempt {
                outcome: RecordedOutcome::Failure(FailureClass::RateLimited),
                facts: AttemptFacts {
                    elapsed_ms: 20,
                    ..AttemptFacts::default()
                },
            }],
        ),
        RecordedProvider::new(
            ProviderKind::DeepSeek,
            [RecordedAttempt {
                outcome: RecordedOutcome::Success(report.to_owned()),
                facts: AttemptFacts {
                    elapsed_ms: 30,
                    tokens: Some(60),
                    ..AttemptFacts::default()
                },
            }],
        ),
    ];

    // When the shared incident budget admits exactly three provider attempts.
    let result = runtime.run_recorded(
        &mut providers,
        Reservation {
            provider_calls: 1,
            tokens: 100,
            evidence_queries: 1,
            cost_micro_usd: 0,
        },
        10,
    );

    // Then DeepSeek is terminal and every complete run is journaled.
    assert_eq!(result, Ok(RunStatus::Succeeded(ProviderKind::DeepSeek)));
    let projection = runtime.journal().project();
    assert_eq!(projection.provider_attempts, 3);
    assert_eq!(projection.provider_time_ms, 60);
    assert_eq!(projection.terminal_provider, Some(ProviderKind::DeepSeek));
}

#[test]
fn runtime_stops_fallback_when_the_incident_call_budget_is_exhausted() {
    // Given a two-call budget and three providers that all fail.
    let config = ReasoningConfig {
        budget: BudgetConfig {
            max_provider_calls: 2,
            ..BudgetConfig::default()
        },
        ..ReasoningConfig::default()
    };
    let mut runtime = IncidentRuntime::new(config).expect("runtime config");
    let failure = || RecordedAttempt {
        outcome: RecordedOutcome::Failure(FailureClass::TemporarilyUnavailable),
        facts: AttemptFacts::default(),
    };
    let mut providers = vec![
        RecordedProvider::new(ProviderKind::OpenAi, [failure()]),
        RecordedProvider::new(ProviderKind::Gemini, [failure()]),
        RecordedProvider::new(ProviderKind::DeepSeek, [failure()]),
    ];

    // When the third admission would exceed the shared incident budget.
    let result = runtime.run_recorded(
        &mut providers,
        Reservation {
            provider_calls: 1,
            tokens: 10,
            evidence_queries: 0,
            cost_micro_usd: 0,
        },
        0,
    );

    // Then the runtime fails closed after two journaled attempts.
    assert_eq!(
        result,
        Err(ai_sre::reasoning::runtime::RuntimeError::Coordination(
            CoordinatorError::Budget(BudgetError::ProviderCalls)
        ))
    );
    assert_eq!(runtime.journal().project().provider_attempts, 2);
    assert_eq!(runtime.journal().project().terminal_provider, None);
}

#[test]
fn bootstrap_rejects_zero_process_limits_before_assembling_adapters() {
    // Given deployment configuration with an unsafe zero `gcx` timeout.
    let config = AppConfig {
        gcx_timeout_secs: 0,
        ..AppConfig::default()
    };

    // When startup validation runs.
    let result = config.validate();

    // Then it fails closed without constructing external clients.
    assert_eq!(result, Err(ConfigError::ZeroLimit));
    assert!(bootstrap::build(config).is_err());
}

#[test]
fn bootstrap_builds_valid_non_secret_dependencies() {
    // Given the default versioned policy and resource limits.
    let config = AppConfig::default();

    // When bootstrap validates and assembles the application.
    let application = bootstrap::build(config).expect("default configuration");

    // Then core runtime and read-only adapter boundaries are available.
    assert!(application.runtime.journal().entries().is_empty());
    assert!(format!("{:?}", application.openai_oauth).contains("OpenAiOAuth"));
}
