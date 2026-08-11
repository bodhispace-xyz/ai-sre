//! GIVEN/WHEN/THEN contracts for the bounded Alertmanager HTTP intake.

use ai_sre::transport::{AlertIntake, IntakeConfig, IntakeError};

#[test]
fn intake_accepts_only_the_bounded_alertmanager_webhook() {
    // Given a valid Alertmanager request with one firing alert.
    let body = br#"{"alerts":[{"status":"firing","fingerprint":"fp-1","labels":{"alertname":"ApiDown","service":"api"},"annotations":{}}]}"#;
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
