use std::collections::BTreeSet;

use ai_sre::reasoning::contracts::{ContractError, DiagnosticReport, EvidenceRef};

#[test]
fn diagnostic_report_accepts_only_citations_from_the_evidence_board() {
    let available = BTreeSet::from(["logs-001".to_owned(), "metrics-001".to_owned()]);
    let report = DiagnosticReport {
        summary: "The API is returning elevated 5xx responses.".to_owned(),
        evidence: vec![EvidenceRef {
            evidence_id: "metrics-001".to_owned(),
        }],
    };

    assert_eq!(report.validate_against(&available), Ok(()));
}

#[test]
fn diagnostic_report_rejects_a_citation_not_on_the_evidence_board() {
    let available = BTreeSet::from(["logs-001".to_owned()]);
    let report = DiagnosticReport {
        summary: "The API is returning elevated 5xx responses.".to_owned(),
        evidence: vec![EvidenceRef {
            evidence_id: "invented-001".to_owned(),
        }],
    };

    assert_eq!(
        report.validate_against(&available),
        Err(ContractError::UnknownEvidence("invented-001".to_owned()))
    );
}

#[test]
fn diagnostic_report_rejects_an_empty_summary() {
    let report = DiagnosticReport {
        summary: "  ".to_owned(),
        evidence: Vec::new(),
    };

    assert_eq!(
        report.validate_against(&BTreeSet::new()),
        Err(ContractError::EmptySummary)
    );
}

#[test]
fn diagnostic_report_rejects_missing_evidence() {
    let report = DiagnosticReport {
        summary: "The API is returning elevated 5xx responses.".to_owned(),
        evidence: Vec::new(),
    };

    assert_eq!(
        report.validate_against(&BTreeSet::new()),
        Err(ContractError::MissingEvidence)
    );
}

#[test]
fn provider_json_rejects_unknown_fields() {
    let json = r#"{
        "summary": "The API is returning elevated 5xx responses.",
        "evidence": [{"evidence_id": "metrics-001", "claim": "invented"}]
    }"#;

    assert!(serde_json::from_str::<DiagnosticReport>(json).is_err());
}
