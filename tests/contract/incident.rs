//! GIVEN/WHEN/THEN contracts for incident normalization and durable replay.

use std::collections::BTreeMap;

use ai_sre::reasoning::{
    incident::{AlertSignal, AlertStatus, normalize},
    journal::JournalEvent,
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
