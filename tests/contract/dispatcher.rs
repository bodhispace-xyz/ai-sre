//! GIVEN/WHEN/THEN contracts for single-writer incident dispatch.

use std::collections::BTreeMap;

use ai_sre::{
    reasoning::{
        dispatcher::{DispatchOutcome, IncidentDispatcher},
        incident::{AlertSignal, AlertStatus, normalize},
        storage::JournalStore,
    },
    transport::IntakeBatch,
};

#[test]
fn dispatcher_deduplicates_replayed_incident_signals() {
    // Given a fresh dispatcher and two equivalent firing signals.
    let path = std::env::temp_dir().join(format!("ai-sre-dispatch-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = JournalStore::open(&path).expect("journal");
    let mut dispatcher = IncidentDispatcher::new(store);
    let signal = AlertSignal {
        status: AlertStatus::Firing,
        fingerprint: "fp-dispatch".to_owned(),
        labels: BTreeMap::from([(String::from("alertname"), String::from("ApiDown"))]),
        annotations: BTreeMap::new(),
    };
    let incident = normalize(signal);

    // When the same batch is processed twice.
    let first = dispatcher
        .process(IntakeBatch {
            incidents: vec![incident.clone()],
        })
        .expect("first batch");
    let second = dispatcher
        .process(IntakeBatch {
            incidents: vec![incident],
        })
        .expect("replayed batch");

    // Then only the first signal is accepted as new work.
    assert_eq!(first, vec![DispatchOutcome::Accepted]);
    assert_eq!(second, vec![DispatchOutcome::Deduplicated]);
    let _ = std::fs::remove_file(path);
}

#[test]
fn dispatcher_recovers_and_starts_a_new_episode_after_recurrence() {
    // Given one firing signal, its recovery, and a later firing recurrence.
    let path = std::env::temp_dir().join(format!("ai-sre-lifecycle-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = JournalStore::open(&path).expect("journal");
    let mut dispatcher = IncidentDispatcher::new(store);
    let firing = AlertSignal {
        status: AlertStatus::Firing,
        fingerprint: "fp-lifecycle".to_owned(),
        labels: BTreeMap::from([(String::from("alertname"), String::from("ApiDown"))]),
        annotations: BTreeMap::new(),
    };
    let resolved = AlertSignal {
        status: AlertStatus::Resolved,
        ..firing.clone()
    };

    // When the lifecycle sequence is processed in order.
    let first = normalize(firing);
    let recovery = normalize(resolved.clone());
    let recurrence = normalize(AlertSignal {
        status: AlertStatus::Firing,
        ..resolved
    });
    let first_id = first.incident_id.clone();
    dispatcher
        .process_new(IntakeBatch {
            incidents: vec![first],
        })
        .expect("first firing");
    dispatcher
        .process_new(IntakeBatch {
            incidents: vec![recovery],
        })
        .expect("recovery");
    let next = dispatcher
        .process_new(IntakeBatch {
            incidents: vec![recurrence],
        })
        .expect("recurrence");

    // Then recovery is accepted and recurrence receives a distinct episode ID.
    assert_eq!(next.len(), 1);
    assert_ne!(next[0].incident_id, first_id);
    assert_eq!(next[0].episode, 2);
    let _ = std::fs::remove_file(path);
}
