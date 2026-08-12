//! GIVEN/WHEN/THEN contracts for bounded OTLP trace degradation behavior.

use std::time::{Duration, Instant};

use ai_sre::observability::tracing::{TraceEvent, TraceExporter};

#[tokio::test]
async fn exporter_submission_does_not_wait_for_an_unreachable_tempo() {
    // GIVEN an exporter with a bounded queue and an unreachable Tempo endpoint.
    let exporter = TraceExporter::start(Some("http://127.0.0.1:1/v1/traces".to_owned()), 1)
        .expect("a positive queue capacity starts the exporter");
    let event = || {
        TraceEvent::new(
            "trace",
            "span",
            "incident.investigation",
            "investigation",
            None,
            1,
        )
        .expect("controlled metadata is accepted")
    };

    // WHEN an incident emits events in a tight loop.
    let started = Instant::now();
    for _ in 0..128 {
        let _ = exporter.try_record(event());
    }

    // THEN submission is bounded by local queue work, not the network timeout.
    assert!(started.elapsed() < Duration::from_millis(100));
    assert!(exporter.dropped_total() > 0);
}

#[test]
fn event_contract_excludes_log_bodies_and_uncontrolled_phases() {
    // GIVEN values that could contain a secret or arbitrary query text.
    // WHEN an event is constructed at the observability boundary.
    // THEN the event is rejected before it can enter the exporter queue.
    assert!(
        TraceEvent::new(
            "trace",
            "span",
            "incident.investigation",
            "arbitrary-query",
            None,
            1,
        )
        .is_none()
    );
    assert!(
        TraceEvent::new("trace", "span", "password=secret", "investigation", None, 1,).is_none()
    );
}
