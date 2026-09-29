//! Exercises cancellation and process loss at the responder's validation transport boundary.

use super::*;
use crate::reasoning::storage::ManualValidationState;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

const WIRE: &str = r#"{"nonce":"original","expires_at":1}"#;

#[tokio::test]
async fn failed_admission_after_source_capture_never_reserves_remote_work() {
    use crate::gitops::{artifact::ManualRepairRequest, sandbox::SshWorkerConfig};
    use std::{fs, process::Command};

    // Given a durable candidate and a real committed source, but no worker credentials.
    let repository = std::env::temp_dir().join(format!("u9-admission-{}", std::process::id()));
    fs::create_dir(&repository).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init"]);
    fs::create_dir_all(repository.join("stacks/utility")).unwrap();
    let source = "services:\n  it-tools:\n    image: ghcr.io/corentinth/it-tools:latest\n";
    fs::write(repository.join("stacks/utility/compose.yml"), source).unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Acceptance",
        "-c",
        "user.email=acceptance@example.invalid",
        "commit",
        "-m",
        "fixture",
    ]);
    let base = git(&["rev-parse", "HEAD"]);
    let mut store = JournalStore::open(":memory:").unwrap();
    let receipt = crate::gitops::receipt::fixture();
    store.record_deployment(&receipt, 221).unwrap();
    let scope = JournalContext {
        incident_id: "incident".into(),
        run_id: "run".into(),
    };
    let evidence = crate::reasoning::storage::fixture_evidence(&mut store, "incident", "run");
    let candidate = store
        .prepare_manual_candidate(&ManualRepairRequest {
            deployment_id: receipt.deployment_id(),
            incident_id: "incident",
            run_id: "run",
            expected_base: &base,
            current_base: &base,
            evidence_digest: &evidence,
            source,
            now: 221,
        })
        .unwrap()
        .unwrap();
    let mut worker = SshValidator::new(SshWorkerConfig {
        host: "127.0.0.1".into(),
        user: "validator".into(),
        port: 22222,
        identity_file: repository.join("missing-key"),
        known_hosts: repository.join("missing-hosts"),
    })
    .unwrap();
    let policy = HandoffPolicy {
        max_validation_age_seconds: 300,
        validator_image: format!(
            "ghcr.io/bodhispace-xyz/ai-sre-validator@sha256:{}",
            "a".repeat(64)
        ),
        runtime_digest: format!("sha256:{}", "b".repeat(64)),
        sandbox_limits: Default::default(),
    };
    // When the fresh checkpoint check rejects after source capture, before network dispatch.
    let result = validate_with_recheck(
        &mut store,
        &mut worker,
        ManualValidationRequest {
            candidate: &candidate,
            context: &scope,
            repository: &repository,
            base: &base,
            policy: &policy,
        },
        || async { Err(SandboxError::Failed) },
    )
    .await;
    // Then no attempt is consumed and no transport failure or remote work is journaled.
    assert!(result.is_err());
    assert!(
        store
            .manual_validation_attempt(&candidate.artifact_digest())
            .unwrap()
            .is_none()
    );
    assert_eq!(store.journal().project().manual_validation_reservations, 0);
    fs::remove_dir_all(repository).unwrap();
}

#[test]
fn final_admission_imports_revocation_before_handoff_qualification_is_checked() {
    // Given a deployment that was qualified before the remote validator ran.
    let mut store = JournalStore::open(":memory:").unwrap();
    let original = crate::gitops::receipt::fixture();
    let id = original.deployment_id().to_owned();
    store.record_deployment(&original, 221).unwrap();
    assert!(store.qualified_deployment(&id, 221).unwrap().is_some());
    let mut revoked = crate::gitops::receipt::fixture();
    revoked.wire.revoked = true;
    let admission = crate::gitops::admission::Admission {
        base: "a".repeat(40),
        deployment_id: id.clone(),
        receipts: vec![original, revoked],
        observed_at: 222,
        expires_at: 250,
    };
    // When a fresh final checkpoint includes the revocation that arrived during validation.
    import_admission(&mut store, &admission, &"a".repeat(40), 223).unwrap();
    // Then qualification is durably unavailable to the handoff finalizer, regardless of validation success.
    assert!(store.qualified_deployment(&id, 223).unwrap().is_none());
    assert!(import_admission(&mut store, &admission, &"b".repeat(40), 223).is_err());
    assert!(import_admission(&mut store, &admission, &"a".repeat(40), 250).is_err());
}

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
