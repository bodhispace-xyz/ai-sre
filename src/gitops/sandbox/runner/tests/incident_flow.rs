//! Exercises synthetic incident qualification through real SSH validation and local operator recovery.

use super::*;
use crate::application::{
    manual_repair::{IncidentRepairRequest, prepare_incident_repair, run_incident_repair},
    manual_repair_admin::acceptance_cli,
};

#[tokio::test]
#[ignore = "requires isolated SSH worker, offline image, homelab Git fixture, and built admin CLI; waits for the real request expiry"]
async fn incident_failure_restart_recovery_and_handoff_over_real_transports() {
    // Given protected synthetic qualification, durable incident evidence, and an isolated worker mirror.
    let repository = PathBuf::from(std::env::var("U9_HOMELAB_REPOSITORY").unwrap());
    let mut fixture = Fixture::new("exit 0").await;
    let result = std::process::Command::new("/usr/bin/git")
        .arg("-C")
        .arg(&repository)
        .args(["rev-parse", "HEAD"])
        .env_clear()
        .output()
        .unwrap();
    assert!(result.status.success());
    let base = String::from_utf8(result.stdout).unwrap();
    let context = crate::reasoning::journal::JournalContext {
        incident_id: "flow-incident".into(),
        run_id: "flow-run".into(),
    };
    crate::reasoning::storage::fixture_evidence(
        &mut fixture.store,
        &context.incident_id,
        &context.run_id,
    );
    let policy = crate::gitops::handoff::HandoffPolicy {
        max_validation_age_seconds: 300,
        validator_image: std::env::var("U9_OFFLINE_IMAGE").unwrap(),
        runtime_digest: std::env::var("U9_RUNTIME_DIGEST").unwrap(),
        sandbox_limits: SandboxLimits::default(),
    };
    let request = IncidentRepairRequest {
        deployment_id: fixture.deployment_receipt.deployment_id(),
        context: &context,
        repository: &repository,
        base: base.trim(),
    };
    let candidate = prepare_incident_repair(&mut fixture.store, &request)
        .await
        .unwrap()
        .unwrap();
    let artifact = candidate.artifact_digest();
    let config = || SshWorkerConfig {
        host: "127.0.0.1".into(),
        user: "validator".into(),
        port: 22222,
        identity_file: "/home/validator/u9-ssh-client/key".into(),
        known_hosts: "/home/validator/u9-ssh-client/known_hosts".into(),
    };
    let mut wrong_config = config();
    wrong_config.known_hosts = "/home/validator/u9-ssh-client/wrong_hosts".into();
    // When host authentication fails after the application commits its attempt reservation.
    assert!(
        run_incident_repair(
            &mut fixture.store,
            &mut SshValidator::new(wrong_config).unwrap(),
            &request,
            &policy
        )
        .await
        .is_err()
    );
    fixture.store = JournalStore::open(fixture.root.join("journal.sqlite")).unwrap();
    let mut worker = SshValidator::new(config()).unwrap();
    assert!(matches!(
        run_incident_repair(&mut fixture.store, &mut worker, &request, &policy).await,
        Err(SandboxError::AttemptExists)
    ));
    let inspected = acceptance_cli(&mut fixture.store, &["inspect", &artifact]).await;
    assert!(inspected.status.success());
    let inspected: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
    assert_eq!(inspected["artifact"]["attempt"]["state"], "Uncertain");
    assert!(inspected["artifact"]["handoff_json"].is_null());
    let expected = inspected["expected_request_digest"]
        .as_str()
        .unwrap()
        .to_owned();
    let original_wire = inspected["artifact"]["attempt"]["request_json"]
        .as_str()
        .unwrap()
        .to_owned();
    let original: serde_json::Value = serde_json::from_str(&original_wire).unwrap();
    assert!(
        !acceptance_cli(
            &mut fixture.store,
            &["recover", &artifact, &expected, "too early"]
        )
        .await
        .status
        .success()
    );
    // A real clock is used: recovery cannot bypass the original dispatch lifetime.
    let expires = original["expires_at"].as_u64().unwrap();
    loop {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if now > expires {
            break;
        }
        eprintln!(
            "waiting for original dispatch expiry: {} seconds",
            expires - now + 1
        );
        tokio::time::sleep(Duration::from_secs((expires - now + 1).min(30))).await;
    }
    let recovered = acceptance_cli(
        &mut fixture.store,
        &[
            "recover",
            &artifact,
            &expected,
            "SSH enrollment corrected; deliberate revalidation",
        ],
    )
    .await;
    assert!(recovered.status.success());
    assert!(
        !acceptance_cli(
            &mut fixture.store,
            &["recover", &artifact, &expected, "replayed recovery"]
        )
        .await
        .status
        .success()
    );
    fixture.store = JournalStore::open(fixture.root.join("journal.sqlite")).unwrap();
    // Then deliberate revalidation derives the same candidate and completes the real offline validator.
    let handoff = run_incident_repair(&mut fixture.store, &mut worker, &request, &policy)
        .await
        .unwrap()
        .unwrap();
    fixture.store = JournalStore::open(fixture.root.join("journal.sqlite")).unwrap();
    let delivered = acceptance_cli(&mut fixture.store, &["inspect", &artifact]).await;
    assert!(delivered.status.success());
    let delivered: serde_json::Value = serde_json::from_slice(&delivered.stdout).unwrap();
    assert_eq!(delivered["artifact"]["handoff_json"], handoff.json());
    assert_eq!(delivered["artifact"]["attempt"]["state"], "Validated");
    assert_eq!(delivered["history"][0]["request_json"], original_wire);
    assert_eq!(
        delivered["history"][0]["operator_uid"],
        rustix::process::geteuid().as_raw()
    );
    assert_eq!(delivered["readiness"], "not_assessed");
    let new_request: serde_json::Value = serde_json::from_str(
        delivered["artifact"]["attempt"]["request_json"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_ne!(new_request["nonce"], original["nonce"]);
    let handoff_wire: serde_json::Value = serde_json::from_str(handoff.json()).unwrap();
    assert_eq!(handoff_wire["candidate_digest"], artifact);
    assert_eq!(handoff_wire["remote_checks"], "not_run");
    assert!(
        handoff_wire["description"]
            .as_str()
            .unwrap()
            .contains("operator review required")
    );
    assert!(matches!(
        run_incident_repair(&mut fixture.store, &mut worker, &request, &policy).await,
        Err(SandboxError::AttemptExists)
    ));
    // Revocation blocks the complete entry point; historical delivery does not restore readiness.
    let mut revoked = crate::gitops::receipt::fixture();
    revoked.wire = fixture.deployment_receipt.wire.clone();
    revoked.wire.revoked = true;
    fixture
        .store
        .record_deployment(
            &revoked,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
    assert!(
        run_incident_repair(&mut fixture.store, &mut worker, &request, &policy)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture
            .store
            .manual_repair_artifact(&artifact)
            .unwrap()
            .unwrap()
            .handoff_json
            .as_deref(),
        Some(handoff.json())
    );
}
