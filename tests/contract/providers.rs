//! Provider-neutral diagnostic report acceptance and rejection scenarios.

use std::collections::BTreeSet;

use ai_sre::reasoning::contracts::{ContractError, DiagnosticReport, EvidenceRef};

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
