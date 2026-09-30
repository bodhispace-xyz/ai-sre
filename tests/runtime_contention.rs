//! Measures real SQLite lock contention against HTTP latency on a single-thread Tokio runtime.

use ai_sre::{
    observability::MetricsSnapshot,
    reasoning::{journal::JournalEvent, storage::JournalStore},
    transport::{AlertIntake, IntakeCommand, IntakeConfig},
};
use std::{
    io::{Read, Write},
    time::{Duration, Instant},
};

#[tokio::test(flavor = "current_thread")]
#[ignore = "isolated diagnostic: induces real SQLite contention and prints synthetic latency samples"]
async fn sqlite_contention_compares_inline_and_off_thread_journal_ownership() {
    // Given identical real SQLite contention in an inline and an off-thread experiment.
    for off_thread in [false, true] {
        let mode = if off_thread { "off_thread" } else { "inline" };
        let path = std::env::temp_dir().join(format!(
            "ai-sre-contention-{}-{mode}.sqlite",
            std::process::id()
        ));
        assert!(!path.exists(), "use a fresh diagnostic fixture");
        let mut journal = JournalStore::open(&path).unwrap();
        let metrics = MetricsSnapshot::default();
        metrics.replace_from(journal.journal()).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, _receiver) = tokio::sync::mpsc::channel::<IntakeCommand>(4);
        let server = tokio::spawn(
            AlertIntake::new(IntakeConfig::default())
                .with_bearer_tokens("synthetic-diagnostic-token", Option::<String>::None)
                .serve_with_metrics(listener, sender, metrics),
        );
        let (locked, ready) = tokio::sync::oneshot::channel();
        let lock_path = path.clone();
        let contention = std::thread::spawn(move || {
            let connection = rusqlite::Connection::open(lock_path).unwrap();
            connection.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            connection.execute_batch("COMMIT").unwrap();
        });
        ready.await.unwrap();
        let (sent, request_ready) = tokio::sync::oneshot::channel();
        let (sampled, results) = tokio::sync::oneshot::channel();
        let client = std::thread::spawn(move || {
            let mut samples = Vec::new();
            let mut sent = Some(sent);
            for _ in 0..24 {
                let started = Instant::now();
                let mut stream =
                    std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer synthetic-diagnostic-token\r\nConnection: close\r\n\r\n").unwrap();
                if let Some(sent) = sent.take() {
                    sent.send(()).unwrap();
                }
                let mut response = Vec::new();
                stream.read_to_end(&mut response).unwrap();
                assert!(response.starts_with(b"HTTP/1.1 200"));
                samples.push(started.elapsed().as_micros());
                std::thread::sleep(Duration::from_millis(20));
            }
            sampled.send(samples).unwrap();
        });
        request_ready.await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(10);
        let timer = tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            tokio::time::Instant::now()
                .saturating_duration_since(deadline)
                .as_micros()
        });
        let write = move || {
            let started = Instant::now();
            journal
                .append(JournalEvent::IncidentOpened {
                    incident_id: "synthetic-contention".into(),
                    alert_name: "Synthetic".into(),
                    labels: Default::default(),
                    annotations: Default::default(),
                    event_time: String::new(),
                    source_event_id: String::new(),
                })
                .unwrap();
            (journal, started.elapsed().as_micros())
        };
        // When a journal append waits behind an actual SQLite writer lock.
        let (journal, write_micros) = if off_thread {
            tokio::task::spawn_blocking(write).await.unwrap()
        } else {
            write()
        };
        let timer_delay = timer.await.unwrap();
        let mut samples = tokio::time::timeout(Duration::from_secs(3), results)
            .await
            .unwrap()
            .unwrap();
        contention.join().unwrap();
        client.join().unwrap();
        // Then all HTTP responses and the durable append succeed; report measured latency.
        samples.sort_unstable();
        assert_eq!(samples.len(), 24);
        assert_eq!(journal.journal().entries().len(), 1);
        println!(
            "mode={mode} requests={} http_p50_us={} http_p95_us={} http_max_us={} timer_delay_us={timer_delay} journal_write_us={write_micros}",
            samples.len(),
            samples[11],
            samples[22],
            samples[23]
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        drop(journal);
        assert_eq!(
            JournalStore::open(&path).unwrap().journal().entries().len(),
            1
        );
        std::fs::remove_file(&path).unwrap();
    }
}
