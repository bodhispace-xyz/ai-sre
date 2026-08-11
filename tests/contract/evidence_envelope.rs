//! Contract tests for immutable, redacted evidence envelopes.
//!
//! These tests describe the security boundary between read-only adapters and
//! provider context: evidence is bounded, secrets are removed, and failures
//! cannot mutate the previously committed board.

use ai_sre::reasoning::evidence::{
    EvidenceBoard, EvidenceError, EvidenceSource, MAX_EVIDENCE_PAYLOAD_BYTES,
    MAX_EVIDENCE_QUERY_BYTES, MAX_EVIDENCE_RECORDS,
};

#[test]
fn evidence_is_redacted_before_it_can_be_cited() {
    // Given a result containing JSON secrets, bearer credentials, and a query secret.
    let mut board = EvidenceBoard::default();
    let query = "{service=\"api\"} password=super-secret";
    let payload = br#"{"message":"Bearer live-token","password":"db-secret","ok":true}"#;

    // When the adapter commits the result to the immutable board.
    let evidence_id = board
        .commit(EvidenceSource::GrafanaLogs, query, payload.to_vec())
        .expect("evidence should commit");

    // Then the citation is stable and neither payload nor query retains secret values.
    let record = &board.records()[0];
    assert_eq!(evidence_id, "evidence-0001");
    let visible = format!(
        "{} {}",
        record.query,
        String::from_utf8_lossy(&record.payload)
    );
    assert!(!visible.contains("super-secret"));
    assert!(!visible.contains("live-token"));
    assert!(!visible.contains("db-secret"));
    assert!(visible.contains("[REDACTED]"));
}

#[test]
fn binary_and_oversized_results_fail_closed_without_board_mutation() {
    // Given a board with one valid record and an oversized adapter result.
    let mut board = EvidenceBoard::default();
    board
        .commit(EvidenceSource::GrafanaMetrics, "up", b"{}".to_vec())
        .expect("seed evidence should commit");
    let before = board.records().to_vec();

    // When binary and oversized results are presented to the evidence boundary.
    let binary = board.commit(EvidenceSource::GrafanaLogs, "logs", vec![0, 159, 146, 150]);
    let oversized = board.commit(
        EvidenceSource::GrafanaMetrics,
        "up",
        vec![b'x'; MAX_EVIDENCE_PAYLOAD_BYTES + 1],
    );

    // Then binary content is replaced with a bounded marker, while oversized content is rejected.
    assert_eq!(binary, Ok("evidence-0002".to_owned()));
    assert_eq!(oversized, Err(EvidenceError::PayloadTooLarge));
    assert_eq!(board.records()[0..1], before[..]);
    assert!(board.records()[1].payload.len() < 64);
}

#[test]
fn evidence_record_limit_is_enforced() {
    // Given a board filled to its immutable record limit.
    let mut board = EvidenceBoard::default();
    for index in 0..MAX_EVIDENCE_RECORDS {
        board
            .commit(
                EvidenceSource::GrafanaMetrics,
                format!("up{{instance=\"{index}\"}}"),
                b"ok".to_vec(),
            )
            .expect("record should fit within the board limit");
    }

    // When one more result is presented.
    let result = board.commit(EvidenceSource::GrafanaMetrics, "up", b"extra".to_vec());

    // Then the board rejects it and preserves all prior citations.
    assert_eq!(result, Err(EvidenceError::RecordLimitExceeded));
    assert_eq!(board.records().len(), MAX_EVIDENCE_RECORDS);
}

#[test]
fn oversized_query_provenance_is_rejected_without_mutation() {
    // Given a model-authored provenance expression larger than the envelope bound.
    let mut board = EvidenceBoard::default();
    let query = "x".repeat(MAX_EVIDENCE_QUERY_BYTES + 1);

    // When the adapter attempts to commit otherwise small output.
    let result = board.commit(EvidenceSource::GrafanaMetrics, query, b"ok".to_vec());

    // Then no unbounded query can enter the immutable context board.
    assert_eq!(result, Err(EvidenceError::QueryTooLarge));
    assert!(board.records().is_empty());
}

#[test]
fn redaction_covers_camel_case_keys_embedded_labels_and_preserves_layout() {
    // Given credentials in common structured, query, and plaintext forms.
    let mut board = EvidenceBoard::default();
    let query = "{password=\"query-secret\"}\n\tservice=\"api\"";
    let payload = br#"{"apiKey":"key-secret","message":"Authorization: Basic basic-secret","log":"line one\nline two"}"#;

    // When the evidence boundary redacts the result.
    board
        .commit(EvidenceSource::GrafanaLogs, query, payload.to_vec())
        .expect("evidence should commit");

    // Then all credential values are removed while non-secret layout remains readable.
    let record = &board.records()[0];
    let visible = format!(
        "{} {}",
        record.query,
        String::from_utf8_lossy(&record.payload)
    );
    for secret in ["query-secret", "key-secret", "basic-secret"] {
        assert!(!visible.contains(secret), "secret leaked: {secret}");
    }
    assert!(record.query.contains('\n'));
    assert!(record.query.contains('\t'));
    assert!(String::from_utf8_lossy(&record.payload).contains("line one\\nline two"));
}
