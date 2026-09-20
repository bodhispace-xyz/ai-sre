//! Exercises cancellation and process loss at the responder's validation transport boundary.

use super::*;
use crate::reasoning::storage::ManualValidationState;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

const WIRE: &str = r#"{"nonce":"original","expires_at":1}"#;

#[tokio::test]
async fn cancelling_response_wait_retains_unknown_outcome_without_invented_timing() {
    // Given a reserved request and a transport that never supplies an authenticated result.
    let mut store = JournalStore::open(":memory:").unwrap();
    store.reserve_manual_validation("candidate", WIRE).unwrap();
    // When the caller cancels the live wait before any result can be recorded.
    assert!(
        tokio::time::timeout(
            Duration::from_millis(1),
            receive_validation(&mut store, "candidate", WIRE, std::future::pending(),)
        )
        .await
        .is_err()
    );
    // Then no cleanup, failure, or duration is invented and implicit retry remains blocked.
    let attempt = store
        .manual_validation_attempt("candidate")
        .unwrap()
        .unwrap();
    assert_eq!(attempt.state, ManualValidationState::Uncertain);
    assert_eq!(attempt.cleanup_status(), "unknown");
    assert_eq!(
        store.journal().project().manual_validation_failed_responses,
        0
    );
    assert_eq!(
        store.journal().project().manual_validation_timed_receipts,
        0
    );
    assert!(
        !store
            .reserve_manual_validation("candidate", r#"{"nonce":"new"}"#)
            .unwrap()
    );
}

#[tokio::test]
async fn killed_responder_wait_requires_explicit_recovery_after_restart() {
    if let Some(path) = std::env::var_os("AI_SRE_TEST_KILLED_WAIT_JOURNAL") {
        let mut store = JournalStore::open(path).unwrap();
        store.reserve_manual_validation("candidate", WIRE).unwrap();
        let _ = receive_validation(&mut store, "candidate", WIRE, async {
            use std::io::Write;
            println!("reservation-committed");
            std::io::stdout().flush().unwrap();
            std::future::pending().await
        })
        .await;
        panic!("pending transport unexpectedly returned");
    }
    // Given a separate process that has durably reserved work and is awaiting a remote response.
    let root = std::env::temp_dir().join(format!("u9-killed-wait-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("journal.sqlite");
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "application::manual_repair::tests::killed_responder_wait_requires_explicit_recovery_after_restart", "--nocapture"])
        .env("AI_SRE_TEST_KILLED_WAIT_JOURNAL", &path)
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null())
        .kill_on_drop(true).spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("child exited before reservation");
            if line == "reservation-committed" {
                break;
            }
        }
    })
    .await
    .unwrap();
    // When the process is killed without destructors or a graceful shutdown callback.
    child.start_kill().unwrap();
    let exit = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!exit.success());
    drop(lines);
    let mut restarted = JournalStore::open(&path).unwrap();
    // Then restart retains uncertainty, and only explicit recovery permits a fresh nonce.
    assert_eq!(
        restarted
            .manual_validation_attempt("candidate")
            .unwrap()
            .unwrap()
            .cleanup_status(),
        "unknown"
    );
    assert_eq!(
        restarted
            .journal()
            .project()
            .manual_validation_failed_responses,
        0
    );
    let next = r#"{"nonce":"next","expires_at":5}"#;
    assert!(
        !restarted
            .reserve_manual_validation("candidate", next)
            .unwrap()
    );
    let digest = crate::gitops::receipt::digest(WIRE.as_bytes());
    assert!(
        restarted
            .recover_manual_validation("candidate", &digest, 501, 20, "worker inspected", 2)
            .unwrap()
    );
    assert!(
        restarted
            .reserve_manual_validation("candidate", next)
            .unwrap()
    );
    assert!(
        receive_validation(&mut restarted, "candidate", next, async {
            Err(SandboxError::Failed)
        })
        .await
        .is_err()
    );
    let before = restarted.journal().project();
    assert_eq!(before.manual_validation_reservations, 2);
    assert_eq!(before.manual_validation_recoveries, 1);
    assert_eq!(before.manual_validation_failed_responses, 1);
    drop(restarted);
    let reopened = JournalStore::open(&path).unwrap();
    assert_eq!(reopened.journal().project(), before);
    assert_eq!(
        reopened
            .manual_validation_history("candidate")
            .unwrap()
            .len(),
        1
    );
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}
