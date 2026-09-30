//! Exercises the actual service binary's HTTP executor while its journal waits on SQLite.

use ai_sre::reasoning::{journal::JournalEvent, storage::JournalStore};
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn service_answers_metrics_while_durable_webhook_admission_waits_for_sqlite() {
    // Given the real service with synthetic credentials, local-only dependencies, and fresh state.
    let root = std::env::temp_dir().join(format!("ai-sre-owner-service-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("journal.sqlite");
    let config = ai_sre::config::AppConfig {
        gcx_binary: root.join("missing-gcx"),
        openai_cache_path: root.join("missing-auth.json"),
        read_only: ai_sre::config::ReadOnlyConfig {
            git_binary: root.join("missing-git"),
            health_binary: root.join("missing-health"),
            git_repository: root.to_str().unwrap().to_owned(),
            ..Default::default()
        },
        ..Default::default()
    };
    let configuration = root.join("config.json");
    std::fs::write(&configuration, serde_json::to_vec(&config).unwrap()).unwrap();
    drop(JournalStore::open(&path).unwrap());
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reserved.local_addr().unwrap();
    // Retain the synthetic notification listener so this test never contacts an external host.
    let notification = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("https://{}", notification.local_addr().unwrap());
    drop(reserved);
    let mut service = Service(
        Command::new(env!("CARGO_BIN_EXE_ai-sre"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir(&root)
            .env("AI_SRE_JOURNAL_PATH", &path)
            .env("AI_SRE_CONFIG", &configuration)
            .env("AI_SRE_LISTEN_ADDR", address.to_string())
            .env("AI_SRE_ALERTMANAGER_TOKEN", "synthetic-service-token")
            .env("NTFY_ENDPOINT", endpoint)
            .env("NTFY_TOPIC", "synthetic")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let url = format!("http://{address}");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(
                service.0.try_wait().unwrap().is_none(),
                "service startup failed"
            );
            if let Ok(response) = client
                .get(format!("{url}/metrics"))
                .bearer_auth("synthetic-service-token")
                .send()
                .await
                && response.status() == reqwest::StatusCode::OK
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let lock = rusqlite::Connection::open(&path).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let webhook_client = client.clone();
    let webhook_url = format!("{url}/webhooks/alertmanager");
    let webhook = tokio::spawn(async move {
        webhook_client.post(webhook_url).bearer_auth("synthetic-service-token")
            .json(&serde_json::json!({"alerts":[{
                "status":"firing", "fingerprint":"owner-contention",
                "labels":{"alertname":"Synthetic","service":"synthetic"}, "annotations":{},
                "startsAt":"2026-09-30T10:00:00Z", "endsAt":"0001-01-01T00:00:00Z", "generatorURL":""
            }]})).send().await.unwrap()
    });
    // When real webhook admission is blocked on another SQLite writer.
    // Await the owner's pre-dispatch counter, not an assumed scheduling delay. An inline
    // owner cannot serve this observation while it is stuck behind the held writer lock.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let response = client
                .get(format!("{url}/metrics"))
                .bearer_auth("synthetic-service-token")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            if response
                .text()
                .await
                .unwrap()
                .contains("ai_sre_runtime_dispatch_started_total 1\n")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !webhook.is_finished(),
        "no acknowledgement before durable commit"
    );
    let response = client
        .get(format!("{url}/metrics"))
        .bearer_auth("synthetic-service-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert!(
        !webhook.is_finished(),
        "HTTP progress must not fabricate webhook durability"
    );
    // Then releasing SQLite allows the durable acknowledgement, and restart sees its event.
    lock.execute_batch("COMMIT").unwrap();
    assert_eq!(
        webhook.await.unwrap().status(),
        reqwest::StatusCode::ACCEPTED
    );
    drop(service);
    drop(lock);
    drop(notification);
    let reopened = JournalStore::open(&path).unwrap();
    assert!(reopened.journal().entries().iter().any(|entry| matches!(&entry.event,
        JournalEvent::IncidentOpened { incident_id, .. } if incident_id == "incident-owner-contention")));
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}
