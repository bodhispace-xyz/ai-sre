//! GIVEN/WHEN/THEN contracts for single-writer incident dispatch.

use std::collections::BTreeMap;

use ai_sre::{
    reasoning::{
        dispatcher::{DispatchOutcome, IncidentDispatcher},
        incident::{AlertSignal, AlertStatus, normalize},
        journal::JournalEvent,
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
        starts_at: "2026-08-11T10:00:00Z".to_owned(),
        ends_at: String::new(),
        generator_url: String::new(),
    };
    let incident = normalize(signal);

    // When the same batch is processed twice.
    let first = dispatcher
        .process(IntakeBatch {
            incidents: vec![incident.clone()],
        })
        .expect("first batch");
    dispatcher
        .mark_completed(&incident.incident_id)
        .expect("complete incident");
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
        starts_at: "2026-08-11T10:00:00Z".to_owned(),
        ends_at: String::new(),
        generator_url: String::new(),
    };
    let resolved = AlertSignal {
        status: AlertStatus::Resolved,
        ends_at: "2026-08-11T10:05:00Z".to_owned(),
        ..firing.clone()
    };

    // When the lifecycle sequence is processed in order.
    let first = normalize(firing);
    let recovery = normalize(resolved.clone());
    let recurrence = normalize(AlertSignal {
        status: AlertStatus::Firing,
        starts_at: "2026-08-11T10:06:00Z".to_owned(),
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

#[test]
fn dispatcher_records_and_ignores_stale_events_after_restart() {
    // Given an episode whose latest durable event is a later firing.
    let path =
        std::env::temp_dir().join(format!("ai-sre-stale-event-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = JournalStore::open(&path).expect("journal");
    let mut dispatcher = IncidentDispatcher::new(store);
    let firing = AlertSignal {
        status: AlertStatus::Firing,
        fingerprint: "fp-stale".to_owned(),
        labels: BTreeMap::from([(String::from("alertname"), String::from("ApiDown"))]),
        annotations: BTreeMap::new(),
        starts_at: "2026-08-11T10:10:00Z".to_owned(),
        ends_at: String::new(),
        generator_url: String::new(),
    };
    dispatcher
        .process_new(IntakeBatch {
            incidents: vec![normalize(firing.clone())],
        })
        .expect("firing");
    drop(dispatcher);

    // When a restarted dispatcher receives an older recovery event.
    let store = JournalStore::open(&path).expect("reopen journal");
    let mut restarted = IncidentDispatcher::new(store);
    let stale = normalize(AlertSignal {
        status: AlertStatus::Resolved,
        ends_at: "2026-08-11T10:09:00Z".to_owned(),
        ..firing
    });
    let result = restarted
        .process(IntakeBatch {
            incidents: vec![stale],
        })
        .expect("stale event");

    // Then state does not regress and the rejection is durable.
    assert_eq!(result, vec![DispatchOutcome::Deduplicated]);
    assert!(
        restarted
            .journal()
            .journal()
            .entries()
            .iter()
            .any(|entry| matches!(entry.event, JournalEvent::AlertOutOfOrder { .. }))
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn active_duplicate_firing_is_durable_but_not_new_work() {
    // Given two identical firing alerts in one grouped delivery.
    let path = std::env::temp_dir().join(format!(
        "ai-sre-active-duplicate-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut dispatcher = IncidentDispatcher::new(JournalStore::open(&path).expect("journal"));
    let alert = AlertSignal {
        status: AlertStatus::Firing,
        fingerprint: "fp-active-duplicate".to_owned(),
        labels: BTreeMap::from([(String::from("alertname"), String::from("ApiDown"))]),
        annotations: BTreeMap::new(),
        starts_at: "2026-08-11T10:00:00Z".to_owned(),
        ends_at: String::new(),
        generator_url: String::new(),
    };
    let first = normalize(alert.clone());
    let second = normalize(alert);

    // When both arrive before the first investigation completes.
    let work = dispatcher
        .process_new(IntakeBatch {
            incidents: vec![first, second],
        })
        .expect("dispatch");

    // Then only one investigation is scheduled, while the replay is recorded.
    assert_eq!(work.len(), 1);
    assert!(
        dispatcher
            .journal()
            .journal()
            .entries()
            .iter()
            .any(|entry| matches!(entry.event, JournalEvent::IncidentResumed { .. }))
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn later_recovery_and_terminal_recurrence_preserve_episode_identity() {
    // Given an incident that completes, recurs, and then recovers again.
    let path = std::env::temp_dir().join(format!(
        "ai-sre-episode-recovery-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut dispatcher = IncidentDispatcher::new(JournalStore::open(&path).expect("journal"));
    let base = AlertSignal {
        status: AlertStatus::Firing,
        fingerprint: "fp-recovery".to_owned(),
        labels: BTreeMap::from([(String::from("alertname"), String::from("ApiDown"))]),
        annotations: BTreeMap::new(),
        starts_at: "2026-08-11T10:00:00Z".to_owned(),
        ends_at: String::new(),
        generator_url: String::new(),
    };
    let first = normalize(base.clone());
    let first_id = first.incident_id.clone();
    dispatcher
        .process_new(IntakeBatch {
            incidents: vec![first],
        })
        .expect("first firing");
    dispatcher
        .mark_completed(&first_id)
        .expect("complete first");
    dispatcher
        .process_new(IntakeBatch {
            incidents: vec![normalize(AlertSignal {
                starts_at: "2026-08-11T10:05:00Z".to_owned(),
                ..base.clone()
            })],
        })
        .expect("recurrence");
    let recovery = normalize(AlertSignal {
        status: AlertStatus::Resolved,
        ends_at: "2026-08-11T10:06:00Z".to_owned(),
        ..base.clone()
    });
    let recovered = dispatcher
        .process(IntakeBatch {
            incidents: vec![recovery],
        })
        .expect("recovery");
    assert_eq!(recovered, vec![DispatchOutcome::Accepted]);
    assert!(dispatcher.journal().journal().entries().iter().any(|entry| {
        matches!(&entry.event, JournalEvent::IncidentRecovered { incident_id, .. } if incident_id.ends_with("-episode-2"))
    }));
    let third = dispatcher
        .process_new(IntakeBatch {
            incidents: vec![normalize(AlertSignal {
                starts_at: "2026-08-11T10:07:00Z".to_owned(),
                ..base
            })],
        })
        .expect("third episode");

    // Then the next firing is episode three, never a reuse of episode two.
    assert_eq!(third[0].episode, 3);
    assert!(third[0].incident_id.ends_with("-episode-3"));
    let _ = std::fs::remove_file(path);
}
