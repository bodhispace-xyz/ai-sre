//! GIVEN/WHEN/THEN contracts for incident normalization and durable replay.

use std::collections::BTreeMap;

use ai_sre::reasoning::{
    incident::{AlertSignal, AlertStatus, normalize},
    journal::{JournalContext, JournalEvent},
    storage::JournalStore,
};

#[test]
fn duplicate_lifecycle_signals_share_one_incident_identity() {
    // Given firing and recovery signals with the same Alertmanager fingerprint.
    let mut labels = BTreeMap::new();
    labels.insert("alertname".to_owned(), "ApiDown".to_owned());
    let firing = AlertSignal {
        status: AlertStatus::Firing,
        fingerprint: "fp-123".to_owned(),
        labels: labels.clone(),
        annotations: BTreeMap::new(),
    };
    let resolved = AlertSignal {
        status: AlertStatus::Resolved,
        ..firing.clone()
    };

    // When both signals are normalized.
    let first = normalize(firing);
    let recovery = normalize(resolved);

    // Then lifecycle transitions retain one stable incident identity.
    assert_eq!(first.incident_id, recovery.incident_id);
    assert_eq!(first.status, AlertStatus::Firing);
    assert_eq!(recovery.status, AlertStatus::Resolved);
}

#[test]
fn journal_store_replays_entries_and_continues_the_sequence() {
    // Given a new journal file containing one incident fact.
    let path = std::env::temp_dir().join(format!("ai-sre-journal-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut store = JournalStore::open(&path).expect("open journal");
    let sequence = store
        .append(JournalEvent::IncidentOpened {
            incident_id: "incident-fp-123".to_owned(),
            alert_name: "ApiDown".to_owned(),
        })
        .expect("append opening");
    assert_eq!(sequence, 0);
    drop(store);

    // When the process reopens the journal and appends recovery.
    let mut reopened = JournalStore::open(&path).expect("replay journal");
    let sequence = reopened
        .append(JournalEvent::IncidentRecovered {
            incident_id: "incident-fp-123".to_owned(),
        })
        .expect("append recovery");

    // Then the prior fact is replayed and the next sequence is deterministic.
    assert_eq!(sequence, 1);
    assert_eq!(reopened.journal().entries().len(), 2);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("jsonl-wal"));
    let _ = std::fs::remove_file(path.with_extension("jsonl-shm"));
}

#[test]
fn journal_and_notification_intent_commit_atomically() {
    // Given a fresh SQLite journal and a redacted notification intent.
    let path = std::env::temp_dir().join(format!("ai-sre-outbox-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut store = JournalStore::open(&path).expect("open journal");

    // When the lifecycle fact and outbox intent are committed together.
    store
        .append_with_outbox(
            &[JournalEvent::IncidentOpened {
                incident_id: "incident-outbox".to_owned(),
                alert_name: "ApiDown".to_owned(),
            }],
            Some(&ai_sre::reasoning::storage::OutboxMessage {
                delivery_id: "incident-outbox:report".to_owned(),
                body: "redacted report".to_owned(),
            }),
        )
        .expect("atomic commit");
    drop(store);

    // Then both the fact and the pending notification survive reopening.
    let mut reopened = JournalStore::open(&path).expect("reopen journal");
    assert_eq!(reopened.journal().entries().len(), 1);
    assert_eq!(reopened.pending_outbox().expect("outbox").len(), 1);
    assert!(
        reopened
            .mark_outbox_delivered("incident-outbox:report", 42)
            .expect("mark delivered")
    );
    assert!(reopened.pending_outbox().expect("outbox").is_empty());
    let _ = std::fs::remove_file(path);
}

#[test]
fn scoped_journal_facts_survive_restart_with_incident_and_run_identity() {
    // Given a fact committed with an explicit incident episode and run scope.
    let path = std::env::temp_dir().join(format!("ai-sre-scoped-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let context = JournalContext {
        incident_id: "incident-scoped".to_owned(),
        run_id: "run-001".to_owned(),
    };
    let mut store = JournalStore::open(&path).expect("open journal");
    store
        .append_scoped(
            JournalEvent::PhaseStarted {
                phase: ai_sre::reasoning::journal::Phase::Investigation,
                at_ms: 0,
            },
            Some(&context),
        )
        .expect("append scoped fact");
    drop(store);

    // When the journal is reopened after the process boundary.
    let reopened = JournalStore::open(&path).expect("reopen journal");

    // Then the causal fact retains both identities for audit and accounting.
    assert_eq!(reopened.journal().entries()[0].context, Some(context));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cost_reservation_is_atomic_idempotent_and_reconciles_known_usage() {
    // Given two durable budget windows with one shared reservation.
    let path = std::env::temp_dir().join(format!("ai-sre-budget-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut store = JournalStore::open(&path).expect("open journal");

    // When the same reservation is retried and then reconciled with trusted usage.
    store
        .reserve_cost(
            "run-budget",
            400,
            &[("incident:one", 1_000), ("day:2026-08-11", 500)],
            10,
        )
        .expect("reserve budget");
    store
        .reserve_cost(
            "run-budget",
            400,
            &[("incident:one", 1_000), ("day:2026-08-11", 500)],
            11,
        )
        .expect("retry is idempotent");
    assert!(
        store
            .reconcile_cost("run-budget", Some(250))
            .expect("reconcile budget")
    );

    // Then the reservation is released to the trusted actual amount once.
    assert!(
        !store
            .reconcile_cost("run-budget", Some(250))
            .expect("duplicate reconciliation")
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cost_reservation_rejects_overcommitment_without_partial_scope_state() {
    // Given a daily ceiling smaller than the requested worst-case reservation.
    let path = std::env::temp_dir().join(format!(
        "ai-sre-budget-reject-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut store = JournalStore::open(&path).expect("open journal");

    // When the reservation would fit one scope but exceed the other.
    let result = store.reserve_cost(
        "run-rejected",
        400,
        &[("incident:one", 1_000), ("day:2026-08-11", 300)],
        10,
    );

    // Then the whole reservation fails atomically.
    assert!(matches!(
        result,
        Err(ai_sre::reasoning::storage::JournalStoreError::BudgetExhausted)
    ));
    assert!(
        !store
            .reconcile_cost("run-rejected", Some(100))
            .is_ok_and(|reconciled| reconciled)
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn efficiency_projection_rebuilds_per_scoped_run() {
    // Given two runs with overlapping phase names but different scopes.
    let path =
        std::env::temp_dir().join(format!("ai-sre-projection-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut store = JournalStore::open(&path).expect("open journal");
    let first = JournalContext {
        incident_id: "incident-projection".to_owned(),
        run_id: "run-a".to_owned(),
    };
    let second = JournalContext {
        incident_id: "incident-projection".to_owned(),
        run_id: "run-b".to_owned(),
    };
    for (context, end) in [(&first, 10_u64), (&second, 30_u64)] {
        store
            .append_scoped(
                JournalEvent::PhaseStarted {
                    phase: ai_sre::reasoning::journal::Phase::Investigation,
                    at_ms: 0,
                },
                Some(context),
            )
            .expect("append phase start");
        store
            .append_scoped(
                JournalEvent::PhaseFinished {
                    phase: ai_sre::reasoning::journal::Phase::Investigation,
                    at_ms: end,
                },
                Some(context),
            )
            .expect("append phase finish");
    }

    // When the process rebuilds the projection for one run.
    let projection = store.efficiency_projection(&first);

    // Then unrelated run timing cannot inflate this run's metrics.
    assert_eq!(projection.active_machine_ms, 10);
    assert_eq!(store.efficiency_projection(&second).active_machine_ms, 30);
    let _ = std::fs::remove_file(&path);
}
