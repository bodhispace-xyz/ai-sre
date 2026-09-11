//! Checks that repair progress is replayable without implying publication or resolution.

use ai_sre::reasoning::journal::{JournalContext, JournalEvent};
use ai_sre::reasoning::storage::JournalStore;

#[test]
fn backwards_operator_clock_keeps_acknowledgement_but_not_a_fabricated_wait() {
    // Given a server wall clock that moved backwards between artifact creation and pickup.
    let mut journal = ai_sre::reasoning::journal::IncidentJournal::default();
    journal.append(JournalEvent::ManualHandoffAcknowledged {
        handoff_digest: "handoff".into(),
        artifact_digest: "artifact".into(),
        operator_uid: 501,
        operator_gid: 20,
        offered_at_unix_seconds: 100,
        acknowledged_at_unix_seconds: 90,
    });
    // When journal replay rebuilds the operator-wait report.
    let projection = journal.project();
    // Then pickup is counted but there is no measured-zero sample or approval claim.
    assert_eq!(projection.manual_handoff_acknowledgements, 1);
    assert_eq!(projection.manual_handoff_timed_acknowledgements, 0);
    assert_eq!(projection.manual_handoff_wait_seconds, 0);
    let metrics = ai_sre::observability::render_journal_metrics(&journal);
    assert!(metrics.contains("ai_sre_manual_handoff_acknowledgements_total 1\n"));
    assert!(metrics.contains("ai_sre_manual_handoff_timed_acknowledgements_total 0\n"));
    assert!(!metrics.contains("501"));
}

#[test]
fn receipt_timing_distinguishes_measured_zero_from_missing_legacy_data() {
    // Given legacy, measured, and sub-millisecond receipts with unrelated wall-clock timestamps.
    let mut journal = ai_sre::reasoning::journal::IncidentJournal::default();
    for (timestamp, timing) in [(100, None), (50, Some(23)), (900, Some(0))] {
        let mut fact = serde_json::json!({"ManualValidation": {
            "artifact_digest": "artifact", "request_digest": "request",
            "stage": "ReceiptReceived", "at_unix_seconds": timestamp
        }});
        if let Some(elapsed) = timing {
            fact["ManualValidation"]["response_elapsed_ms"] = elapsed.into();
        }
        journal.append(serde_json::from_value(fact).unwrap());
    }
    // When the efficiency view is rebuilt solely from those durable facts.
    let projection = journal.project();
    // Then only measured responses contribute time and the sample count includes measured zero.
    assert_eq!(projection.manual_validation_receipts, 3);
    assert_eq!(projection.manual_validation_timed_receipts, 2);
    assert_eq!(projection.manual_validation_response_ms, 23);
    assert_eq!(projection.active_machine_ms, 0);
    assert_eq!(projection.human_wait_ms, 0);
    let metrics = ai_sre::observability::render_journal_metrics(&journal);
    assert!(metrics.contains("ai_sre_manual_validation_response_ms_total 23\n"));
    assert!(metrics.contains("ai_sre_manual_validation_timed_receipts_total 2\n"));
}

#[test]
fn repair_progress_survives_restart_and_stays_scoped_to_its_run() {
    // Given two runs whose durable facts distinguish preparation from validated handoff.
    let directory = std::env::temp_dir().join(format!(
        "ai-sre-repair-efficiency-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("journal.sqlite");
    let context = JournalContext {
        incident_id: "incident-one".into(),
        run_id: "run-one".into(),
    };
    let other = JournalContext {
        run_id: "run-two".into(),
        ..context.clone()
    };
    let mut store = JournalStore::open(&path).unwrap();
    for scope in [&context, &other] {
        store
            .append_scoped(
                JournalEvent::ManualRepairPrepared {
                    artifact_digest: "candidate".into(),
                    qualification_digest: "qualification".into(),
                    at_unix_seconds: 100,
                },
                Some(scope),
            )
            .unwrap();
    }
    store
        .append_scoped(
            JournalEvent::ManualRepairValidated {
                handoff_digest: "handoff".into(),
                artifact_digest: "candidate".into(),
                at_unix_seconds: 110,
            },
            Some(&context),
        )
        .unwrap();
    let before = store.efficiency_projection(&context);
    drop(store);

    // When a new journal owner rebuilds the report from SQLite, without telemetry services.
    let reopened = JournalStore::open(&path).unwrap();
    let projection = reopened.efficiency_projection(&context);

    // Then only this run's progress is counted; wall-clock gaps do not invent machine or human time.
    assert_eq!(projection, before);
    assert_eq!(projection.manual_repair_preparations, 1);
    assert_eq!(projection.manual_repair_validated_handoffs, 1);
    assert_eq!(projection.active_machine_ms, 0);
    assert_eq!(projection.human_wait_ms, 0);
    assert_eq!(projection.terminal_provider, None);
    assert_eq!(
        reopened
            .efficiency_projection(&other)
            .manual_repair_validated_handoffs,
        0
    );
    assert_eq!(reopened.journal().project().manual_repair_preparations, 2);
    let metrics = ai_sre::observability::render_journal_metrics(reopened.journal());
    assert!(metrics.contains("ai_sre_manual_repair_preparations_total 2\n"));
    assert!(metrics.contains("ai_sre_manual_repair_validated_handoffs_total 1\n"));
    assert!(!metrics.contains("incident-one"));
    assert!(!metrics.contains("candidate"));
    drop(reopened);
    std::fs::remove_dir_all(directory).unwrap();
}
