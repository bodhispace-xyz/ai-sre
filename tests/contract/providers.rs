//! Provider-neutral diagnostic report acceptance and rejection scenarios.

use std::collections::BTreeSet;

use ai_sre::adapters::llm::{
    ApiKey, ProviderFailure, deepseek::DeepSeekClient, gemini::GeminiClient,
};
use ai_sre::reasoning::budget::{BudgetConfig, BudgetError, BudgetState, Reservation};
use ai_sre::reasoning::contracts::{ContractError, DiagnosticReport, EvidenceRef};
use ai_sre::reasoning::coordinator::{CoordinatorError, ReasoningConfig, ReasoningRun, RunStatus};
use ai_sre::reasoning::router::{ProviderKind, ProviderOrder, next_provider, next_provider_in};

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
