//! Exercises the real service process to prove trace failures cannot lose incident reports.

use std::{
    path::PathBuf,
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ai_sre::{
    config::AppConfig,
    reasoning::{journal::JournalEvent, storage::JournalStore},
};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ai-sre-u7-process-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn report_survives(endpoint: Option<&str>, expect_loss: bool) {
    let scratch = Scratch::new();
    let config = AppConfig {
        gcx_binary: "/bin/echo".into(),
        openai_cache_path: scratch.0.join("unused-auth.json"),
        ..Default::default()
    };
    let config_path = scratch.0.join("config.json");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let journal_path = scratch.0.join("journal.sqlite");
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let spawn = || {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ai-sre"));
        command
            .env_clear()
            .env("AI_SRE_CONFIG", &config_path)
            .env("AI_SRE_JOURNAL_PATH", &journal_path)
            .env("AI_SRE_LISTEN_ADDR", address.to_string())
            .env("AI_SRE_ALERTMANAGER_TOKEN", "test-only")
            .env("NTFY_ENDPOINT", "https://127.0.0.1:1")
            .env("NTFY_TOPIC", "test-only")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(endpoint) = endpoint {
            command.env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", endpoint);
        }
        command.spawn().unwrap()
    };

    // Given the actual service, fake read-only evidence, and no admitted paid providers.
    let mut child = spawn();
    let metrics_url = format!("http://{address}/metrics");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "service exited during startup"
            );
            if client
                .get(&metrics_url)
                .bearer_auth("test-only")
                .send()
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    // When an authenticated alert arrives while trace export cannot complete normally.
    let started = std::time::Instant::now();
    let response = client.post(format!("http://{address}/webhooks/alertmanager"))
        .bearer_auth("test-only").json(&serde_json::json!({"alerts":[{
            "status":"firing", "fingerprint":"u7-process", "labels":{"alertname":"ApiDown","service":"api"},
            "annotations":{}, "startsAt":"2026-09-08T00:00:00Z", "endsAt":"0001-01-01T00:00:00Z", "generatorURL":"https://prometheus.example"
        }]})).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let report_url = format!("http://{address}/incidents/incident-u7-process");
    let report = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = client
                .get(&report_url)
                .bearer_auth("test-only")
                .send()
                .await
                .unwrap();
            if response.status().is_success() {
                break response.text().await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("trace failure must not prevent report publication");

    // Then the report and its completion survive process loss and reopen from SQLite.
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        report.contains(&ai_sre::observability::tracing::incident_trace_id(
            "incident-u7-process"
        ))
    );
    if expect_loss {
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let metrics = client
                    .get(&metrics_url)
                    .bearer_auth("test-only")
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap();
                if metrics.contains("ai_sre_trace_dropped_total 1\n") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("export loss must be visible");
    }
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let store = JournalStore::open(&journal_path).unwrap();
    assert!(
        store
            .journal()
            .entries()
            .iter()
            .any(|entry| matches!(&entry.event,
        JournalEvent::IncidentCompleted { incident_id } if incident_id == "incident-u7-process"))
    );
    assert_eq!(store.stored_reports(1).unwrap()[0].html, report);
    drop(store);
    let mut restarted = spawn();
    let restored = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(response) = client
                .get(&report_url)
                .bearer_auth("test-only")
                .send()
                .await
            {
                if response.status().is_success() {
                    break response.text().await.unwrap();
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(restored, report);
    restarted.kill().await.unwrap();
    restarted.wait().await.unwrap();
}

#[tokio::test]
async fn actual_incident_survives_disabled_unavailable_slow_and_rejecting_export() {
    report_survives(None, false).await;
    report_survives(Some("http://127.0.0.1:1/v1/traces"), true).await;
    for slow in [false, true] {
        // Given either a storage-full-style rejection or a stalled ingestion server.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/v1/traces",
                    axum::routing::post(move || async move {
                        if slow {
                            std::future::pending::<()>().await;
                        }
                        axum::http::StatusCode::INSUFFICIENT_STORAGE
                    }),
                ),
            )
            .await
            .unwrap();
        });
        report_survives(Some(&endpoint), true).await;
        server.abort();
    }
}

#[tokio::test]
#[ignore = "requires disposable storage-full Tempo; never target a production backend"]
async fn actual_incident_survives_real_storage_full_tempo() {
    // Given an operator-prepared disposable Tempo whose data filesystem is full.
    let endpoint =
        std::env::var("AI_SRE_TEST_FULL_OTLP").expect("explicit disposable endpoint required");
    // When the real service investigates and exports its completed span.
    // Then its report and journal survive even if Tempo acknowledges before disk flush fails.
    report_survives(Some(&endpoint), false).await;
}
