//! GIVEN/WHEN/THEN contracts for the bounded Alertmanager HTTP intake.

use ai_sre::{
    observability::MetricsSnapshot,
    reasoning::{journal::JournalEvent, storage::JournalStore},
    transport::{AlertIntake, IntakeCommand, IntakeConfig, IntakeError},
};

#[test]
fn intake_accepts_only_the_bounded_alertmanager_webhook() {
    // Given a valid Alertmanager request with one firing alert.
    let body = br#"{"version":"4","groupKey":"{}:{alertname=\"ApiDown\"}","truncatedAlerts":0,"status":"firing","receiver":"ai-sre","groupLabels":{"alertname":"ApiDown"},"commonLabels":{"service":"api"},"commonAnnotations":{},"externalURL":"https://alertmanager.example","alerts":[{"status":"firing","fingerprint":"fp-1","labels":{"alertname":"ApiDown","service":"api"},"annotations":{},"startsAt":"2026-08-11T10:00:00Z","endsAt":"0001-01-01T00:00:00Z","generatorURL":"https://prometheus"}]}"#;
    let request = format!(
        "POST /webhooks/alertmanager HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        std::str::from_utf8(body).expect("JSON is UTF-8")
    );

    // When the narrow intake parses it.
    let batch = AlertIntake::new(IntakeConfig::default())
        .parse_request(request.as_bytes())
        .expect("valid webhook");

    // Then one stable incident reaches the application boundary.
    assert_eq!(batch.incidents.len(), 1);
    assert_eq!(batch.incidents[0].incident_id, "incident-fp-1");
}

#[tokio::test]
async fn authenticated_fragmented_request_waits_for_durable_ack() {
    // Given an authenticated intake and a worker that controls durability.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<IntakeCommand>(1);
    let server = tokio::spawn(
        AlertIntake::new(IntakeConfig::default())
            .with_bearer_tokens("current-secret", Some("next-secret"))
            .serve(listener, sender),
    );
    let body = br#"{"alerts":[{"status":"firing","fingerprint":"fp-auth","labels":{"alertname":"ApiDown"},"annotations":{},"startsAt":"2026-08-11T10:00:00Z","endsAt":"0001-01-01T00:00:00Z","generatorURL":"https://prometheus"}]}"#;
    let request = format!(
        "POST /webhooks/alertmanager HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer current-secret\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        std::str::from_utf8(body).expect("JSON is UTF-8")
    );
    let split = request.len() / 2;
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");

    // When the valid request arrives in multiple TCP fragments.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(&request.as_bytes()[..split])
        .await
        .expect("first fragment");
    stream
        .write_all(&request.as_bytes()[split..])
        .await
        .expect("second fragment");
    let command = receiver.recv().await.expect("queued command");
    assert_eq!(command.batch.incidents[0].incident_id, "incident-fp-auth");
    command.acknowledged.send(Ok(())).expect("acknowledge");

    // Then HTTP 202 is emitted only after the worker confirms durable admission.
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("response");
    assert!(response.starts_with(b"HTTP/1.1 202"));
    server.abort();
}

#[tokio::test]
async fn metrics_endpoint_requires_auth_and_exposes_fixed_aggregate_names() {
    // Given an authenticated intake with a journal-derived metrics snapshot.
    let path = std::env::temp_dir().join(format!(
        "ai-sre-transport-metrics-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut journal = JournalStore::open(&path).expect("open journal");
    journal
        .append(JournalEvent::IncidentOpened {
            incident_id: "private-id".to_owned(),
            alert_name: "ApiDown".to_owned(),
            event_time: String::new(),
            source_event_id: String::new(),
        })
        .expect("append incident");
    let metrics = MetricsSnapshot::default();
    metrics.replace_from(journal.journal()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let (sender, _receiver) = tokio::sync::mpsc::channel::<IntakeCommand>(1);
    let server = tokio::spawn(
        AlertIntake::new(IntakeConfig::default())
            .with_bearer_tokens("metrics-secret", Option::<String>::None)
            .serve_with_metrics(listener, sender, metrics),
    );
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");

    // When the metrics request supplies the valid bearer credential.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(
            b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer metrics-secret\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("response");

    // Then only fixed aggregate names are exposed, never incident labels.
    let response = String::from_utf8(response).expect("UTF-8 response");
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains("ai_sre_incidents_opened_total 1"));
    assert!(!response.contains("private-id"));
    server.abort();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn intake_rejects_other_routes_and_oversized_bodies() {
    // Given a request aimed at another route and a body over the configured limit.
    let wrong_route = b"POST /admin HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}";
    let oversized = b"POST /webhooks/alertmanager HTTP/1.1\r\nContent-Length: 10\r\n\r\n{}";
    let intake = AlertIntake::new(IntakeConfig { max_body_bytes: 4 });

    // When both requests cross the HTTP boundary.
    let wrong_route_result = intake.parse_request(wrong_route);
    let oversized_result = intake.parse_request(oversized);

    // Then neither request reaches normalization.
    assert_eq!(wrong_route_result, Err(IntakeError::NotAllowed));
    assert_eq!(oversized_result, Err(IntakeError::BodyTooLarge));
}

#[test]
fn intake_rejects_unknown_alert_fields_before_durable_admission() {
    // Given an otherwise valid webhook containing an undeclared alert field.
    let body = br#"{"alerts":[{"status":"firing","fingerprint":"fp-unknown","labels":{},"annotations":{},"secret":"must-not-cross-boundary"}]}"#;
    let request = format!(
        "POST /webhooks/alertmanager HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        std::str::from_utf8(body).expect("JSON is UTF-8")
    );

    // When the bounded parser validates the nested alert schema.
    let result = AlertIntake::new(IntakeConfig::default()).parse_request(request.as_bytes());

    // Then malformed source data is rejected before normalization or queueing.
    assert_eq!(result, Err(IntakeError::Malformed));
}
