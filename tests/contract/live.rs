//! GIVEN/WHEN/THEN contracts for bounded live provider execution.

use ai_sre::reasoning::{
    budget::{BudgetConfig, Reservation},
    coordinator::ReasoningConfig,
    evidence::EvidenceSource,
    live::{LiveProviders, run_live},
    router::{ProviderKind, ProviderOrder},
    runtime::IncidentRuntime,
};

#[tokio::test]
async fn live_runner_uses_deterministic_baseline_when_external_providers_are_absent() {
    // Given a finite runtime with only the deterministic provider enabled and bounded evidence.
    let mut runtime = IncidentRuntime::new(ReasoningConfig {
        provider_order: ProviderOrder {
            providers: vec![ProviderKind::Deterministic],
        },
        budget: BudgetConfig::default(),
        max_tool_turns: 4,
    })
    .expect("runtime");
    runtime
        .commit_evidence(
            EvidenceSource::GrafanaLogs,
            "{service=\"api\"}",
            b"error".to_vec(),
            0,
        )
        .expect("evidence");

    // When the live runner executes without API clients.
    let status = run_live(
        &mut runtime,
        LiveProviders {
            openai: None,
            gemini: None,
            deepseek: None,
        },
        "diagnose this incident",
        Reservation {
            provider_calls: 1,
            tokens: 100,
            evidence_queries: 1,
            cost_micro_usd: 0,
        },
        1,
    )
    .await
    .expect("deterministic run");

    // Then it returns a cited report through the same terminal runtime path.
    assert_eq!(
        status,
        ai_sre::reasoning::coordinator::RunStatus::Succeeded(ProviderKind::Deterministic)
    );
    assert!(runtime.last_report().is_some());
}
