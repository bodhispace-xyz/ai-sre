//! GIVEN/WHEN/THEN contracts for safe shadow-report notification rendering.

use std::collections::BTreeSet;

use ai_sre::{
    adapters::ntfy::{render_message, render_message_with_view},
    reasoning::{
        contracts::DiagnosticReport, coordinator::RunStatus, investigation::InvestigationResult,
        router::ProviderKind,
    },
};

#[test]
fn notification_contains_report_context_but_no_execution_authority() {
    // Given a successful shadow report with evidence references.
    let result = InvestigationResult {
        incident_id: "incident-fp-123".to_owned(),
        evidence_ids: BTreeSet::from(["evidence-0001".to_owned()]),
        status: RunStatus::Succeeded(ProviderKind::OpenAi),
        report: DiagnosticReport {
            summary: "API error rate is elevated.".to_owned(),
            evidence: Vec::new(),
        },
    };

    // When the operator message is rendered.
    let message = render_message(&result);

    // Then it contains bounded context and explicitly states that no action ran.
    assert!(message.contains("incident-fp-123"));
    assert!(message.contains("API error rate is elevated."));
    assert!(message.contains("Mode: shadow; no action executed."));
    assert!(!message.contains("command:"));

    // And the optional operator link is encoded as one stable path segment.
    let linked = render_message_with_view(&result, Some("https://sre.example/incidents"));
    assert!(linked.contains("View: https://sre.example/incidents/incident-fp-123"));
}
