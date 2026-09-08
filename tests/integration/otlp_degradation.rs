//! Verifies that trace export failures and unsafe metadata cannot affect callers.

use std::time::{Duration, Instant};

use ai_sre::observability::tracing::{TraceEvent, TraceExporter};

#[tokio::test]
#[ignore = "requires disposable Tempo on loopback ports 14318 and 13200"]
async fn real_tempo_accepts_and_returns_redacted_application_span() {
    // Given a disposable Tempo with persistent storage and the production config.
    let otlp = std::env::var("AI_SRE_TEST_OTLP")
        .unwrap_or_else(|_| "http://127.0.0.1:14318/v1/traces".into());
    let query =
        std::env::var("AI_SRE_TEST_TEMPO").unwrap_or_else(|_| "http://127.0.0.1:13200".into());
    let exporter = TraceExporter::start(Some(otlp), 4).unwrap();
    let incident = "u7-sensitive-canary-incident";
    let trace_id = ai_sre::observability::tracing::incident_trace_id(incident);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();

    // When the actual application exporter submits a completed investigation.
    exporter.try_record(
        TraceEvent::new(
            incident,
            "u7-sensitive-canary-run",
            "incident.investigation",
            "investigation",
            Some("openai"),
            25,
        )
        .unwrap(),
    );

    // Then the backend returns the span by ID without raw correlation inputs.
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let response = client
                .get(format!("{query}/api/traces/{trace_id}"))
                .send()
                .await
                .unwrap();
            if response.status().is_success() {
                let body = response.text().await.unwrap();
                assert!(body.contains("incident.investigation"));
                assert!(!body.contains("sensitive-canary"));
                assert!(body.contains("ai-sre"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(exporter.dropped_total(), 0);
    println!("retrieved trace {trace_id}");
}

#[test]
fn endpoint_resolution_obeys_signal_precedence_and_preserves_base_paths() {
    use ai_sre::observability::tracing::resolve_endpoint;
    // Given a deployment using a path prefix and a trace-specific override.
    let base = "https://tempo.example/otel/";
    let specific = "https://tempo.example/custom";

    // When endpoints are resolved without network access.
    let derived = resolve_endpoint(Some(base), None);
    let overridden = resolve_endpoint(Some(base), Some(specific));

    // Then only base URLs gain the signal suffix, and unsafe inputs disable export.
    assert_eq!(
        derived.as_deref(),
        Some("https://tempo.example/otel/v1/traces")
    );
    assert_eq!(overridden.as_deref(), Some(specific));
    assert_eq!(resolve_endpoint(None, None), None);
    for invalid in [
        "file:///tmp/traces",
        "https://user:secret@tempo.example",
        "https://tempo.example/?token=secret",
        "https://tempo.example/#secret",
        "",
    ] {
        assert_eq!(resolve_endpoint(Some(base), Some(invalid)), None);
    }
}

#[test]
fn trace_limits_are_backward_compatible_and_reject_unbounded_values() {
    use ai_sre::config::{AppConfig, TracingConfig};
    // Given an existing configuration with no tracing section.
    let mut old = serde_json::to_value(AppConfig::default()).unwrap();
    old.as_object_mut().unwrap().remove("tracing");

    // When loading it or supplying invalid trace resource ceilings.
    let loaded = AppConfig::from_json(&old.to_string()).unwrap();

    // Then defaults preserve compatibility while every resource remains bounded.
    assert_eq!(loaded.tracing, TracingConfig::default());
    for limits in [
        TracingConfig {
            queue_capacity: 0,
            ..Default::default()
        },
        TracingConfig {
            queue_capacity: 4097,
            ..Default::default()
        },
        TracingConfig {
            timeout_ms: 0,
            ..Default::default()
        },
        TracingConfig {
            timeout_ms: 10001,
            ..Default::default()
        },
        TracingConfig {
            max_response_bytes: 0,
            ..Default::default()
        },
        TracingConfig {
            max_response_bytes: 65537,
            ..Default::default()
        },
    ] {
        let config = AppConfig {
            tracing: limits,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
    assert!(TraceExporter::configured(None, TracingConfig::default()).is_none());
}

#[test]
fn terminal_status_controls_outcome_and_incident_lookup_identity() {
    use ai_sre::reasoning::{coordinator::RunStatus, router::ProviderKind};
    // Given a completed Gemini fallback and an exhausted investigation.
    for (status, provider, outcome) in [
        (
            RunStatus::Succeeded(ProviderKind::Gemini),
            "gemini",
            "succeeded",
        ),
        (RunStatus::Exhausted, "deterministic", "exhausted"),
    ] {
        // When status is lowered into trace metadata.
        let event = TraceEvent::new(
            "incident",
            "run",
            "incident.investigation",
            "investigation",
            None,
            5,
        )
        .unwrap()
        .with_status(status);
        let value = serde_json::to_value(event).unwrap();

        // Then labels come from domain enums and the displayed lookup ID matches export.
        assert_eq!(value["provider"], provider);
        assert_eq!(value["outcome"], outcome);
        assert_eq!(
            value["trace_id"],
            ai_sre::observability::tracing::incident_trace_id("incident")
        );
    }
}

#[tokio::test]
async fn receiver_gets_timed_otlp_spans_with_service_identity() {
    // Given an OTLP receiver that captures the actual HTTP request body.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1/traces", listener.local_addr().unwrap());
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/v1/traces",
                axum::routing::post(move |body: axum::body::Bytes| {
                    let sender = sender.clone();
                    async move {
                        sender.send(body).await.unwrap();
                        ([("content-type", "application/json")], "{}")
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let exporter = TraceExporter::start(Some(endpoint), 1).unwrap();

    // When a completed investigation is exported with a controlled provider alias.
    assert!(
        exporter.try_record(
            TraceEvent::new(
                "incident",
                "run",
                "incident.investigation",
                "investigation",
                Some("openai"),
                25
            )
            .unwrap()
        )
    );
    let body = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // Then Tempo receives valid correlation widths, a real interval, and no raw identifiers.
    let span = &payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
    assert_eq!(span["traceId"].as_str().unwrap().len(), 32);
    assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
    let start: u64 = span["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
    let end: u64 = span["endTimeUnixNano"].as_str().unwrap().parse().unwrap();
    assert!(start > 0);
    assert_eq!(end - start, 25_000_000);
    assert_eq!(
        payload["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"],
        "ai-sre"
    );
    assert!(payload.to_string().contains("openai"));
    server.abort();
}

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

#[tokio::test]
async fn tempo_http_rejections_and_partial_success_are_counted_as_loss() {
    for (status, body) in [
        (503, "{}"),
        (200, r#"{"partialSuccess":{"rejectedSpans":"1"}}"#),
        (200, "invalid-json"),
        (200, r#"{"partialSuccess":"invalid"}"#),
        (504, "{}"),
    ] {
        // Given a receiver that rejects the single submitted span.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/v1/traces",
                    axum::routing::post(move || async move {
                        if status == 504 {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                        (axum::http::StatusCode::from_u16(status).unwrap(), body)
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let exporter = TraceExporter::start(Some(endpoint), 1).unwrap();

        // When transport succeeds but Tempo rejects or cannot acknowledge the span.
        assert!(
            exporter.try_record(
                TraceEvent::new(
                    "incident",
                    "run",
                    "incident.reasoning",
                    "reasoning",
                    None,
                    1
                )
                .unwrap()
            )
        );

        // Then loss becomes visible without retrying or blocking incident work.
        tokio::time::timeout(Duration::from_secs(3), async {
            while exporter.dropped_total() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(exporter.dropped_total(), 1);
        let metrics = ai_sre::observability::MetricsSnapshot::with_traces(Some(exporter));
        assert!(
            metrics
                .read()
                .await
                .contains("ai_sre_trace_dropped_total 1\n")
        );
        server.abort();
    }
}

#[test]
fn correlation_is_hex_encoded_and_raw_metadata_is_not_serialized() {
    // Given incident identifiers containing content that must not enter Tempo.
    let event = TraceEvent::new(
        "secret-incident",
        "secret-run",
        "incident.reasoning",
        "reasoning",
        Some("openai"),
        25,
    )
    .unwrap();

    // When metadata is serialized through the validated event boundary.
    let value = serde_json::to_value(event).unwrap();
    let encoded = value.to_string();

    // Then correlation uses fixed-width hex and arbitrary provider values are rejected.
    assert_eq!(value["trace_id"].as_str().unwrap().len(), 32);
    assert_eq!(value["span_id"].as_str().unwrap().len(), 16);
    assert!(!encoded.contains("secret"));
    assert!(
        TraceEvent::new(
            "incident",
            "run",
            "incident.reasoning",
            "reasoning",
            Some("secret"),
            1
        )
        .is_none()
    );
}
