//! Connects an explicitly enrolled pilot alert to bounded manual repair preparation.
//!
//! Root-owned configuration and fresh publisher checkpoints supply authority. Model text never
//! selects the repository, deployment, worker, or patch. Each incident permits one automatic attempt.

use super::manual_repair::{
    IncidentRepairRequest, ManualValidationRequest, prepare_incident_repair, validate_with_recheck,
};
use crate::{
    gitops::{
        admission::{self, Admission},
        handoff::HandoffPolicy,
        inbox::{InboxLimits, ReceiptInbox},
        receipt::read_protected,
        sandbox::{SandboxError, SandboxPlan, SshValidator, SshWorkerConfig},
    },
    reasoning::{
        incident::{AlertStatus, IncidentSignal},
        journal::{JournalContext, JournalEvent},
        storage::JournalStore,
    },
};
use serde::Deserialize;
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: String,
    repository: PathBuf,
    checkpoint: PathBuf,
    inbox: PathBuf,
    max_checkpoint_age_seconds: u64,
    max_inbox_entries: usize,
    max_inbox_bytes: u64,
    alert_name: String,
    worker: SshWorkerConfig,
    policy: HandoffPolicy,
}

pub(crate) struct RepairDispatch {
    repository: PathBuf,
    checkpoint: PathBuf,
    inbox: ReceiptInbox,
    max_age: u64,
    alert_name: String,
    worker: SshValidator,
    policy: HandoffPolicy,
}

fn now() -> Result<u64, SandboxError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| SandboxError::Failed)
}

impl RepairDispatch {
    pub(crate) async fn from_environment() -> Result<Option<Self>, SandboxError> {
        let Some(path) = std::env::var_os("AI_SRE_MANUAL_REPAIR_CONFIG") else {
            return Ok(None);
        };
        let bytes =
            tokio::task::spawn_blocking(move || read_protected(&PathBuf::from(path), 16 * 1024))
                .await
                .map_err(|_| SandboxError::InvalidConfiguration)?
                .map_err(|_| SandboxError::InvalidConfiguration)?;
        let config: Config =
            serde_json::from_slice(&bytes).map_err(|_| SandboxError::InvalidConfiguration)?;
        Self::configured(config).map(Some)
    }

    fn configured(config: Config) -> Result<Self, SandboxError> {
        if config.schema != "ai-sre/manual-repair-config/v1"
            || !config.repository.is_absolute()
            || !config.checkpoint.is_absolute()
            || !(1..=300).contains(&config.max_checkpoint_age_seconds)
            || config.alert_name.is_empty()
            || config.alert_name.len() > 128
            || config.alert_name.chars().any(char::is_control)
            || !(1..=3600).contains(&config.policy.max_validation_age_seconds)
        {
            return Err(SandboxError::InvalidConfiguration);
        }
        SandboxPlan::new(
            &config.policy.validator_image,
            config.policy.sandbox_limits.clone(),
        )?;
        let inbox = ReceiptInbox::new(
            config.inbox,
            InboxLimits {
                entries: config.max_inbox_entries,
                bytes: config.max_inbox_bytes,
            },
        )
        .map_err(|_| SandboxError::InvalidConfiguration)?;
        Ok(Self {
            repository: config.repository,
            checkpoint: config.checkpoint,
            inbox,
            max_age: config.max_checkpoint_age_seconds,
            alert_name: config.alert_name,
            worker: SshValidator::new(config.worker)?,
            policy: config.policy,
        })
    }

    pub(crate) async fn run(
        &mut self,
        journal: &mut JournalStore,
        incident: &IncidentSignal,
        context: &JournalContext,
    ) -> Result<Option<String>, SandboxError> {
        if !selected(incident, &self.alert_name) {
            return Ok(None);
        }
        if context.incident_id != incident.incident_id || context.run_id.is_empty() {
            return Err(SandboxError::InvalidConfiguration);
        }
        if already_attempted(journal, &context.incident_id) {
            return Ok(Some("Manual repair: an attempt already exists for this incident; inspect it before explicit recovery.".into()));
        }
        let admission =
            load_checkpoint(self.checkpoint.clone(), self.inbox.clone(), self.max_age).await?;
        for receipt in &admission.receipts {
            journal
                .record_deployment(receipt, now()?)
                .map_err(|_| SandboxError::Failed)?;
            tokio::task::yield_now().await;
        }
        ensure_current(&admission, now()?)?;
        let request = IncidentRepairRequest {
            deployment_id: &admission.deployment_id,
            context,
            repository: &self.repository,
            base: &admission.base,
        };
        let Some(candidate) = prepare_incident_repair(journal, &request).await? else {
            return Ok(Some(
                "Manual repair: no eligible image correction; recommendation only.".into(),
            ));
        };
        let artifact = candidate.artifact_digest();
        let checkpoint = self.checkpoint.clone();
        let inbox = self.inbox.clone();
        let max_age = self.max_age;
        let recheck = || {
            let checkpoint = checkpoint.clone();
            let inbox = inbox.clone();
            let expected = &admission;
            async move {
                let current = load_checkpoint(checkpoint, inbox, max_age).await?;
                same_selection(expected, &current)?;
                Ok(Some(current))
            }
        };
        let outcome = validate_with_recheck(
            journal,
            &mut self.worker,
            ManualValidationRequest {
                candidate: &candidate,
                context,
                repository: &self.repository,
                base: &admission.base,
                policy: &self.policy,
            },
            recheck,
        )
        .await;
        match outcome {
            Ok(Some(_)) => Ok(Some(format!(
                "Manual repair artifact {artifact} is available through ai-sre-admin inspect. Operator review required; remote CI has not run. No PR or deployment was created."
            ))),
            Ok(None) => Ok(Some(format!(
                "Manual repair candidate {artifact} is not ready; current qualification or evidence was rejected."
            ))),
            Err(_) => Ok(Some(format!(
                "Manual repair candidate {artifact} is not ready. Inspect its durable attempt before retry; remote cleanup may be unknown."
            ))),
        }
    }
}

fn selected(incident: &IncidentSignal, alert_name: &str) -> bool {
    incident.status == AlertStatus::Firing
        && incident.alert_name == alert_name
        && incident.labels.get("service").map(String::as_str) == Some("it-tools")
}

fn already_attempted(journal: &JournalStore, incident: &str) -> bool {
    journal.journal().entries().iter().any(|entry| {
        entry
            .context
            .as_ref()
            .is_some_and(|c| c.incident_id == incident)
            && matches!(entry.event, JournalEvent::ManualValidation { .. })
    })
}

fn ensure_current(admission: &Admission, at: u64) -> Result<(), SandboxError> {
    if at < admission.observed_at || at >= admission.expires_at {
        return Err(SandboxError::Failed);
    }
    Ok(())
}

fn same_selection(before: &Admission, after: &Admission) -> Result<(), SandboxError> {
    if before.base != after.base || before.deployment_id != after.deployment_id {
        return Err(SandboxError::Failed);
    }
    Ok(())
}

async fn load_checkpoint(
    path: PathBuf,
    inbox: ReceiptInbox,
    age: u64,
) -> Result<Admission, SandboxError> {
    let at = now()?;
    let admission = tokio::task::spawn_blocking(move || admission::read(&path, &inbox, at, age))
        .await
        .map_err(|_| SandboxError::Failed)?
        .map_err(|_| SandboxError::Failed)?;
    ensure_current(&admission, now()?)?;
    Ok(admission)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reasoning::{
        incident::{AlertSignal, normalize},
        journal::ManualValidationStage,
    };

    fn incident(service: &str, name: &str, status: AlertStatus) -> IncidentSignal {
        normalize(AlertSignal {
            status,
            fingerprint: "pilot".into(),
            labels: [
                ("service".into(), service.into()),
                ("alertname".into(), name.into()),
            ]
            .into(),
            annotations: Default::default(),
            starts_at: "2026-09-29T00:00:00Z".into(),
            ends_at: String::new(),
            generator_url: String::new(),
        })
    }

    #[test]
    fn only_the_enrolled_firing_pilot_alert_can_select_a_repair() {
        // Given an operator enrollment for the image-drift alert, not arbitrary IT Tools failures.
        let name = "ItToolsImageDrift";
        // When incoming service, alert kind, or lifecycle state differs.
        assert!(!selected(
            &incident("jellyfin", name, AlertStatus::Firing),
            name
        ));
        assert!(!selected(
            &incident("it-tools", "DiskFull", AlertStatus::Firing),
            name
        ));
        assert!(!selected(
            &incident("it-tools", name, AlertStatus::Resolved),
            name
        ));
        // Then only the exact firing pilot route is eligible; free-form diagnosis is never parsed.
        assert!(selected(
            &incident("it-tools", name, AlertStatus::Firing),
            name
        ));
    }

    #[test]
    fn a_previous_run_prevents_another_automatic_attempt_for_the_same_incident() {
        // Given a durable reservation from a previous reasoning run of this incident.
        let mut journal = JournalStore::open(":memory:").unwrap();
        let context = JournalContext {
            incident_id: "incident".into(),
            run_id: "previous".into(),
        };
        journal
            .append_scoped(
                JournalEvent::ManualValidation {
                    artifact_digest: "artifact".into(),
                    request_digest: "request".into(),
                    stage: ManualValidationStage::Reserved,
                    at_unix_seconds: Some(221),
                    response_elapsed_ms: None,
                },
                Some(&context),
            )
            .unwrap();
        // When dispatch considers a new run, the incident identity still carries the reservation.
        assert!(already_attempted(&journal, "incident"));
        // Then an unrelated incident remains eligible for its own bounded attempt.
        assert!(!already_attempted(&journal, "other"));
    }

    #[test]
    fn refreshed_admission_cannot_switch_source_or_deployment_during_validation() {
        let admission = || Admission {
            base: "a".repeat(40),
            deployment_id: "deployment".into(),
            receipts: Vec::new(),
            observed_at: 100,
            expires_at: 120,
        };
        // Given an admitted source and deployment at dispatch time.
        let before = admission();
        // When a later checkpoint changes either selection or expires during a blocking read.
        let mut moved = admission();
        moved.base = "b".repeat(40);
        assert!(same_selection(&before, &moved).is_err());
        moved = admission();
        moved.deployment_id = "another".into();
        assert!(same_selection(&before, &moved).is_err());
        assert!(ensure_current(&before, 99).is_err());
        assert!(ensure_current(&before, 120).is_err());
        // Then stable selection within its exact time window remains eligible.
        assert!(same_selection(&before, &admission()).is_ok());
        assert!(ensure_current(&before, 119).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires root in a disposable Linux VM; creates only synthetic admission evidence"]
    async fn protected_dispatch_reserves_once_and_restart_does_not_retry() {
        use std::{fs, os::unix::fs::PermissionsExt, process::Command};
        assert!(rustix::process::geteuid().is_root());
        // Given a protected checkpoint, real Git source, and synthetic qualified pilot evidence.
        let root = PathBuf::from(format!("/root/ai-sre-dispatch-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let live_repository = std::env::var_os("U9_LIVE_DISPATCH_REPOSITORY").map(PathBuf::from);
        let live = live_repository.is_some();
        let repository = live_repository.unwrap_or_else(|| root.join("repository"));
        if !live {
            fs::create_dir(&repository).unwrap();
        }
        let git = |args: &[&str]| {
            let result = Command::new("/usr/bin/git")
                .arg("-C")
                .arg(&repository)
                .args(args)
                .env_clear()
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(result.status.success(), "{result:?}");
            String::from_utf8(result.stdout).unwrap().trim().to_owned()
        };
        if !live {
            git(&["init"]);
            fs::create_dir_all(repository.join("stacks/utility")).unwrap();
            fs::write(
                repository.join("stacks/utility/compose.yml"),
                format!(
                    "services:\n  it-tools:\n    image: ghcr.io/corentinth/it-tools@sha256:{}\n",
                    "c".repeat(64)
                ),
            )
            .unwrap();
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
        }
        let base = git(&["rev-parse", "HEAD"]);
        let at = now().unwrap();
        let mut receipt = crate::gitops::receipt::fixture();
        receipt.wire.completed_at = at - 122;
        receipt.wire.policy_declared_at = at - 130;
        for (sample, offset) in receipt.wire.samples.iter_mut().zip([121, 61, 1]) {
            sample.observed_at = at - offset;
        }
        let inbox = root.join("receipts");
        fs::create_dir(&inbox).unwrap();
        fs::write(
            inbox.join("qualified.json"),
            serde_json::to_vec(&receipt.wire).unwrap(),
        )
        .unwrap();
        let checkpoint = root.join("checkpoint.json");
        fs::write(&checkpoint, serde_json::to_vec(&serde_json::json!({
            "schema": "ai-sre/repair-checkpoint/v1", "repository": "bodhispace-xyz/bodhispace-homelab",
            "base": base, "deployment_id": receipt.deployment_id(),
            "observed_at": at, "expires_at": at + 300,
            "receipt_digests": [receipt.wire.digest()],
        })).unwrap()).unwrap();
        let (image, runtime) = if live {
            // Disposable test identities are copied only inside the isolated VM, never printed.
            fs::copy(
                "/home/validator/u9-ssh-client/key",
                root.join("missing-key"),
            )
            .unwrap();
            fs::set_permissions(root.join("missing-key"), fs::Permissions::from_mode(0o600))
                .unwrap();
            fs::copy(
                "/home/validator/u9-ssh-client/known_hosts",
                root.join("missing-hosts"),
            )
            .unwrap();
            (
                std::env::var("U9_OFFLINE_IMAGE").unwrap(),
                std::env::var("U9_RUNTIME_DIGEST").unwrap(),
            )
        } else {
            (
                format!(
                    "ghcr.io/bodhispace-xyz/ai-sre-validator@sha256:{}",
                    "a".repeat(64)
                ),
                format!("sha256:{}", "b".repeat(64)),
            )
        };
        let config = serde_json::json!({
            "schema": "ai-sre/manual-repair-config/v1", "repository": repository,
            "checkpoint": checkpoint, "inbox": inbox, "max_checkpoint_age_seconds": 300,
            "max_inbox_entries": 256, "max_inbox_bytes": 16777216,
            "alert_name": "ItToolsImageDrift",
            "worker": {"host":"127.0.0.1", "user":"validator", "port":22222,
                "identity_file":root.join("missing-key"), "known_hosts":root.join("missing-hosts")},
            "policy": {"max_validation_age_seconds":300,
                "validator_image":image, "runtime_digest":runtime, "sandbox_limits":{}}
        });
        let mut dispatch =
            RepairDispatch::configured(serde_json::from_value(config).unwrap()).unwrap();
        fs::set_permissions(&checkpoint, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(
            load_checkpoint(checkpoint.clone(), dispatch.inbox.clone(), 300)
                .await
                .is_err()
        );
        fs::set_permissions(&checkpoint, fs::Permissions::from_mode(0o644)).unwrap();
        let signal = incident("it-tools", "ItToolsImageDrift", AlertStatus::Firing);
        let mut scope = JournalContext {
            incident_id: signal.incident_id.clone(),
            run_id: "first-run".into(),
        };
        let database = root.join("journal.sqlite");
        let mut journal = JournalStore::open(&database).unwrap();
        crate::reasoning::storage::fixture_evidence(
            &mut journal,
            &scope.incident_id,
            &scope.run_id,
        );
        // When dispatch reserves the candidate and reaches either the absent or enrolled test worker.
        let note = dispatch
            .run(&mut journal, &signal, &scope)
            .await
            .unwrap()
            .unwrap();
        assert!(
            note.contains(if live {
                "available through"
            } else {
                "not ready"
            }),
            "{note}"
        );
        assert_eq!(
            journal.journal().project().manual_validation_reservations,
            1
        );
        assert_eq!(
            journal.journal().project().manual_repair_validated_handoffs,
            u64::from(live)
        );
        drop(journal);
        let mut journal = JournalStore::open(&database).unwrap();
        scope.run_id = "after-restart".into();
        fs::remove_file(&checkpoint).unwrap();
        // Then a new reasoning run neither needs admission nor creates another external attempt.
        let note = dispatch
            .run(&mut journal, &signal, &scope)
            .await
            .unwrap()
            .unwrap();
        assert!(note.contains("already exists"));
        assert_eq!(
            journal.journal().project().manual_validation_reservations,
            1
        );
        drop(journal);
        fs::remove_dir_all(root).unwrap();
    }
}
