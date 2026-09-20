//! Commits protected deployment qualification and its audit fact in one SQLite transaction.

mod handoff;

use super::{JournalStore, JournalStoreError};
use crate::{
    gitops::{
        qualified_deployment::{QualifiedDeployment, assess_health},
        receipt::{ProtectedReceipt, ReceiptWire},
    },
    reasoning::journal::{DeploymentQualificationReason, JournalEntry, JournalEvent},
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

fn eligible(wire: &ReceiptWire, now: u64) -> bool {
    wire.deployment_succeeded
        && !wire.revoked
        && wire.policy_declared_at < wire.completed_at
        && wire.commit == wire.observed_commit
        && wire.image == wire.observed_image
        && assess_health(&wire.policy, wire.completed_at, now, &wire.samples).is_ok()
}

struct EvidenceBinding {
    digest: String,
    citations: Vec<crate::gitops::artifact::EvidenceCitation>,
}

fn evidence_binding(
    connection: &rusqlite::Connection,
    scope: &crate::reasoning::journal::JournalContext,
) -> Result<Option<EvidenceBinding>, JournalStoreError> {
    let mut statement = connection.prepare("SELECT event_json FROM journal_events WHERE incident_id=?1 AND run_id=?2 AND json_type(event_json, '$.EvidenceCommitted')='object' ORDER BY sequence LIMIT 65")?;
    let rows = statement.query_map(params![scope.incident_id, scope.run_id], |row| {
        row.get::<_, String>(0)
    })?;
    let mut records = Vec::new();
    let mut ids = std::collections::BTreeSet::new();
    for row in rows {
        let event: JournalEvent = serde_json::from_str(&row?)?;
        let JournalEvent::EvidenceCommitted {
            evidence_id,
            source,
            content_digest: Some(digest),
            ..
        } = event
        else {
            return Ok(None);
        };
        if !ids.insert(evidence_id.clone())
            || records.len() >= crate::reasoning::evidence::MAX_EVIDENCE_RECORDS
            || !digest.strip_prefix("sha256:").is_some_and(|value| {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        {
            return Ok(None);
        }
        records.push((evidence_id, source, digest));
    }
    if records.is_empty() {
        return Ok(None);
    }
    let digest = crate::gitops::receipt::digest(&serde_json::to_vec(&(scope, &records))?);
    let citations = records
        .into_iter()
        .map(
            |(evidence_id, source, content_digest)| crate::gitops::artifact::EvidenceCitation {
                evidence_id,
                source,
                content_digest,
            },
        )
        .collect();
    Ok(Some(EvidenceBinding { digest, citations }))
}

#[cfg(test)]
pub(crate) fn fixture_evidence(
    store: &mut JournalStore,
    incident_id: &str,
    run_id: &str,
) -> String {
    use crate::reasoning::{
        evidence::{EvidenceBoard, EvidenceSource},
        journal::JournalContext,
    };
    let mut board = EvidenceBoard::default();
    let id = board
        .commit(
            EvidenceSource::Health,
            "fixture health",
            b"healthy".to_vec(),
        )
        .unwrap();
    let context = JournalContext {
        incident_id: incident_id.into(),
        run_id: run_id.into(),
    };
    store
        .append_scoped(
            JournalEvent::EvidenceCommitted {
                evidence_id: id,
                source: EvidenceSource::Health,
                content_digest: Some(board.records()[0].content_digest()),
                at_ms: 1,
            },
            Some(&context),
        )
        .unwrap();
    store.repair_evidence_digest(&context).unwrap().unwrap()
}

impl JournalStore {
    /// Returns an identity for the complete durable evidence set in exactly this incident/run.
    /// Legacy or missing content digests fail closed; no model-supplied digest is trusted.
    pub fn repair_evidence_digest(
        &self,
        context: &crate::reasoning::journal::JournalContext,
    ) -> Result<Option<String>, JournalStoreError> {
        Ok(evidence_binding(&self.connection, context)?.map(|binding| binding.digest))
    }
    /// Prepares and journals a candidate from current durable qualification.
    /// Missing qualification or invalid scope yields recommendation-only (`None`).
    /// This method never runs scripts, exports credentials, or marks validation passed.
    pub fn prepare_manual_candidate(
        &mut self,
        request: &crate::gitops::artifact::ManualRepairRequest<'_>,
    ) -> Result<Option<crate::gitops::artifact::ManualRepairCandidate>, JournalStoreError> {
        let Some(qualified) = self.qualified_deployment(request.deployment_id, request.now)? else {
            return Ok(None);
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let scope = crate::reasoning::journal::JournalContext {
            incident_id: request.incident_id.into(),
            run_id: request.run_id.into(),
        };
        let Some(binding) = evidence_binding(&tx, &scope)? else {
            return Ok(None);
        };
        if binding.digest != request.evidence_digest {
            return Ok(None);
        }
        let Some(candidate) = crate::gitops::artifact::ManualRepairCandidate::build(
            request,
            &qualified,
            binding.citations,
        ) else {
            return Ok(None);
        };
        let digest = candidate.artifact_digest();
        let encoded = candidate.json();
        // Recheck the projection while holding the writer lock; another importer
        // may have invalidated the qualification between read and candidate construction.
        let still_eligible: bool = tx.query_row(
            "SELECT eligible FROM qualified_deployments WHERE deployment_id=?1",
            [request.deployment_id],
            |r| r.get(0),
        )?;
        if !still_eligible {
            return Ok(None);
        }
        let existing: Option<String> = tx
            .query_row(
                "SELECT candidate_json FROM manual_repair_candidates WHERE artifact_digest=?1",
                [&digest],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing != encoded {
                return Err(JournalStoreError::InvalidDeployment);
            }
            let recorded = self.journal.entries().iter().any(|entry| {
                matches!(&entry.event, JournalEvent::ManualRepairPrepared { artifact_digest, qualification_digest, .. }
                    if artifact_digest == &digest && qualification_digest == &qualified.receipt_digest())
                    && entry.context.as_ref().is_some_and(|scope| scope.incident_id == request.incident_id && scope.run_id == request.run_id)
            });
            if !recorded {
                return Err(JournalStoreError::InvalidDeployment);
            }
            return Ok(Some(candidate));
        }
        let sequence = self.journal.entries().len() as u64;
        let sql_sequence =
            i64::try_from(sequence).map_err(|_| JournalStoreError::NonContiguousSequence)?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM journal_events", [], |r| r.get(0))?;
        if count != sql_sequence {
            return Err(JournalStoreError::NonContiguousSequence);
        }
        let event = JournalEvent::ManualRepairPrepared {
            artifact_digest: digest.clone(),
            qualification_digest: qualified.receipt_digest(),
            at_unix_seconds: request.now,
        };
        tx.execute(
            "INSERT INTO manual_repair_candidates(artifact_digest,candidate_json) VALUES (?1,?2)",
            params![digest, encoded],
        )?;
        tx.execute("INSERT INTO journal_events(sequence,event_json,incident_id,run_id) VALUES (?1,?2,?3,?4)",
            params![sql_sequence, serde_json::to_string(&event)?, request.incident_id, request.run_id])?;
        tx.commit()?;
        self.journal.restore(JournalEntry {
            sequence,
            context: Some(crate::reasoning::journal::JournalContext {
                incident_id: request.incident_id.into(),
                run_id: request.run_id.into(),
            }),
            event,
        });
        Ok(Some(candidate))
    }

    /// Atomically stores a protected receipt and its qualification audit fact.
    /// Exact replays report current eligibility without changing durable state.
    /// A contradictory receipt or failed initial assessment permanently invalidates
    /// the producer identity; a new deployment requires a new identity.
    pub fn record_deployment(
        &mut self,
        receipt: &ProtectedReceipt,
        now: u64,
    ) -> Result<bool, JournalStoreError> {
        let wire = &receipt.wire;
        wire.validate_shape()
            .map_err(|_| JournalStoreError::InvalidDeployment)?;
        let digest = wire.digest();
        let encoded = serde_json::to_string(wire)?;
        let mut accepted = eligible(wire, now);
        let mut reason = if wire.revoked {
            DeploymentQualificationReason::Revoked
        } else if accepted {
            DeploymentQualificationReason::Qualified
        } else {
            DeploymentQualificationReason::FailedInitialAssessment
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<(String, bool)> = tx.query_row(
            "SELECT receipt_digest, eligible FROM qualified_deployments WHERE deployment_id = ?1",
            [&wire.deployment_id], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        if let Some((old_digest, old_eligible)) = previous {
            if !old_eligible {
                return Ok(false);
            }
            let audited =
                self.journal
                    .entries()
                    .iter()
                    .rev()
                    .find_map(|entry| match &entry.event {
                        JournalEvent::DeploymentQualification {
                            deployment_id,
                            receipt_digest,
                            eligible,
                            ..
                        } if deployment_id == &wire.deployment_id => {
                            Some((receipt_digest, eligible))
                        }
                        _ => None,
                    });
            if !matches!(audited, Some((recorded, true)) if recorded == &old_digest) {
                return Err(JournalStoreError::InvalidDeployment);
            }
            if old_digest == digest {
                // Aging or clock correction changes eligibility, not the receipt's identity.
                return Ok(accepted);
            }
            accepted = false;
            reason = if wire.revoked {
                DeploymentQualificationReason::Revoked
            } else {
                DeploymentQualificationReason::ContradictoryReceipt
            };
        }
        let event = JournalEvent::DeploymentQualification {
            deployment_id: wire.deployment_id.clone(),
            receipt_digest: digest.clone(),
            eligible: accepted,
            reason: Some(reason),
            at_unix_seconds: now,
        };
        let sequence = self.journal.entries().len() as u64;
        let sql_sequence =
            i64::try_from(sequence).map_err(|_| JournalStoreError::NonContiguousSequence)?;
        let stored_count: i64 =
            tx.query_row("SELECT COUNT(*) FROM journal_events", [], |row| row.get(0))?;
        if sql_sequence != stored_count {
            return Err(JournalStoreError::NonContiguousSequence);
        }
        tx.execute("INSERT INTO deployment_receipt_audit(receipt_digest,receipt_json) VALUES (?1,?2) ON CONFLICT(receipt_digest) DO NOTHING", params![digest, encoded])?;
        tx.execute(
            "INSERT INTO qualified_deployments(deployment_id, receipt_digest, receipt_json, eligible) VALUES (?1,?2,?3,?4)
             ON CONFLICT(deployment_id) DO UPDATE SET eligible = 0",
            params![wire.deployment_id, digest, encoded, accepted],
        )?;
        tx.execute(
            "INSERT INTO journal_events(sequence,event_json) VALUES (?1,?2)",
            params![sql_sequence, serde_json::to_string(&event)?],
        )?;
        tx.commit()?;
        self.journal.restore(JournalEntry {
            sequence,
            context: None,
            event,
        });
        Ok(accepted)
    }

    /// Rechecks freshness and policy on a durable record; never qualifies arbitrary caller JSON.
    /// The database has the same service-owned trust boundary as the incident journal.
    pub fn qualified_deployment(
        &self,
        id: &str,
        now: u64,
    ) -> Result<Option<QualifiedDeployment>, JournalStoreError> {
        let row: Option<(String, String)> = self.connection.query_row(
            "SELECT receipt_json,receipt_digest FROM qualified_deployments WHERE deployment_id=?1 AND eligible=1",
            [id], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        let Some((json, digest)) = row else {
            return Ok(None);
        };
        let fact = self
            .journal
            .entries()
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                JournalEvent::DeploymentQualification {
                    deployment_id,
                    receipt_digest,
                    eligible,
                    at_unix_seconds,
                    ..
                } if deployment_id == id => Some((receipt_digest, eligible, at_unix_seconds)),
                _ => None,
            });
        if !matches!(fact, Some((recorded, true, at)) if recorded == &digest && now >= *at) {
            return Err(JournalStoreError::InvalidDeployment);
        }
        if json.len() > 1024 * 1024 {
            return Err(JournalStoreError::InvalidDeployment);
        }
        let wire: ReceiptWire = serde_json::from_str(&json)?;
        wire.validate_shape()
            .map_err(|_| JournalStoreError::InvalidDeployment)?;
        if wire.deployment_id != id || wire.digest() != digest {
            return Err(JournalStoreError::InvalidDeployment);
        }
        Ok(eligible(&wire, now).then_some(QualifiedDeployment { wire }))
    }
}

#[cfg(test)]
mod tests {
    use crate::{gitops::receipt::fixture, reasoning::storage::JournalStore};

    #[test]
    fn evidence_binding_survives_reopen_and_rejects_legacy_facts() {
        use crate::reasoning::{
            evidence::EvidenceSource,
            journal::{JournalContext, JournalEvent},
        };
        // Given the same redacted evidence committed under two distinct run scopes.
        let path =
            std::env::temp_dir().join(format!("u9-evidence-binding-{}.sqlite", std::process::id()));
        let mut store = JournalStore::open(&path).unwrap();
        let first = super::fixture_evidence(&mut store, "incident", "run-a");
        let second = super::fixture_evidence(&mut store, "incident", "run-b");
        assert_ne!(first, second);
        drop(store);
        // When the application reopens its durable journal rather than reusing an in-memory board.
        let mut store = JournalStore::open(&path).unwrap();
        let scope = JournalContext {
            incident_id: "incident".into(),
            run_id: "run-a".into(),
        };
        // Then the binding is stable, but legacy evidence without content identity cannot qualify.
        assert_eq!(
            store.repair_evidence_digest(&scope).unwrap().as_deref(),
            Some(first.as_str())
        );
        store
            .append_scoped(
                JournalEvent::EvidenceCommitted {
                    evidence_id: "evidence-0002".into(),
                    source: EvidenceSource::Health,
                    content_digest: None,
                    at_ms: 2,
                },
                Some(&scope),
            )
            .unwrap();
        assert!(store.repair_evidence_digest(&scope).unwrap().is_none());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn identical_receipt_replays_do_not_turn_clock_changes_into_revocations() {
        // Given an already qualified receipt with one durable qualification fact.
        let receipt = fixture();
        let mut store = JournalStore::open(":memory:").unwrap();
        assert!(store.record_deployment(&receipt, 221).unwrap());
        // When identical evidence is replayed outside its valid time interval.
        assert!(!store.record_deployment(&receipt, 821).unwrap());
        assert!(!store.record_deployment(&receipt, 219).unwrap());
        // Then current ineligibility creates no revocation, and a corrected clock can reassess it.
        assert_eq!(store.journal().entries().len(), 1);
        assert!(
            store
                .qualified_deployment(receipt.deployment_id(), 222)
                .unwrap()
                .is_some()
        );
        assert!(store.record_deployment(&receipt, 222).unwrap());
        assert_eq!(store.journal().entries().len(), 1);
    }

    #[test]
    fn prepared_snapshot_is_traversable_under_restrictive_service_umask() {
        // Given a separate process so the restrictive umask cannot affect concurrent tests.
        let executable = std::env::current_exe().unwrap();
        // When the real candidate/snapshot contract runs under a service-style umask of 077.
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", "umask 077; exec \"$1\" --exact reasoning::storage::gitops::tests::manual_candidate_binds_the_qualified_receipt_and_does_not_claim_validation --nocapture", "u9-umask"])
            .arg(executable).output().unwrap();
        // Then source directories remain traversable without exposing the outer private directory.
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn failed_audit_commit_cannot_leave_a_qualified_projection() {
        // Given SQLite fault injection at the audit-event insert, after the projection write.
        let mut store = JournalStore::open(":memory:").unwrap();
        store.connection.execute_batch("CREATE TRIGGER refuse_event BEFORE INSERT ON journal_events BEGIN SELECT RAISE(ABORT, 'fault'); END;").unwrap();
        let receipt = fixture();
        // When qualification cannot commit its authoritative audit fact.
        assert!(store.record_deployment(&receipt, 221).is_err());
        // Then both the projection and the in-memory journal remain unchanged.
        assert!(
            store
                .qualified_deployment(receipt.deployment_id(), 222)
                .unwrap()
                .is_none()
        );
        assert!(store.journal().entries().is_empty());
        let audit_count: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM deployment_receipt_audit", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(audit_count, 0);
    }

    #[test]
    fn qualification_rejects_failed_revoked_late_policy_and_stale_receipts() {
        // Given protected receipts whose provenance alone cannot prove healthy deployment.
        for case in 0..4 {
            let mut receipt = fixture();
            match case {
                0 => receipt.wire.deployment_succeeded = false,
                1 => receipt.wire.revoked = true,
                2 => receipt.wire.policy_declared_at = receipt.wire.completed_at,
                _ => receipt.wire.samples[1].gatus_healthy = false,
            }
            let mut store = JournalStore::open(":memory:").unwrap();
            // When each receipt is assessed for the pilot.
            assert!(!store.record_deployment(&receipt, 221).unwrap());
            // Then none becomes an available rollback source.
            assert!(
                store
                    .qualified_deployment(receipt.deployment_id(), 222)
                    .unwrap()
                    .is_none()
            );
        }
        let receipt = fixture();
        let mut store = JournalStore::open(":memory:").unwrap();
        assert!(store.record_deployment(&receipt, 221).unwrap());
        assert!(
            store
                .qualified_deployment(receipt.deployment_id(), 821)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn manual_candidate_binds_the_qualified_receipt_and_does_not_claim_validation() {
        use crate::gitops::artifact::ManualRepairRequest;
        // Given a durable qualified deployment and an exact pilot source revision.
        let receipt = fixture();
        let mut store = JournalStore::open(":memory:").unwrap();
        store.record_deployment(&receipt, 221).unwrap();
        let source = "services:\n  it-tools:\n    image: ghcr.io/corentinth/it-tools:latest\n    restart: unless-stopped\n";
        let request = ManualRepairRequest {
            deployment_id: receipt.deployment_id(),
            incident_id: "incident-1",
            run_id: "run-1",
            expected_base: &"d".repeat(40),
            current_base: &"d".repeat(40),
            evidence_digest: &format!("sha256:{}", "e".repeat(64)),
            source,
            now: 222,
        };
        // When a syntactically valid digest has no durable evidence in this incident/run.
        assert!(store.prepare_manual_candidate(&request).unwrap().is_none());
        let evidence = super::fixture_evidence(&mut store, "incident-1", "run-1");
        let request = ManualRepairRequest {
            evidence_digest: &evidence,
            ..request
        };
        // Evidence from another scope cannot be reused merely by copying its digest.
        let foreign = ManualRepairRequest {
            incident_id: "other-incident",
            ..request
        };
        assert!(store.prepare_manual_candidate(&foreign).unwrap().is_none());
        // When the manual candidate is prepared and replayed.
        let candidate = store.prepare_manual_candidate(&request).unwrap().unwrap();
        let replay = store.prepare_manual_candidate(&request).unwrap().unwrap();
        // Then identity is stable and neither validation nor remote publication is invented.
        assert_eq!(candidate.artifact_digest(), replay.artifact_digest());
        assert!(candidate.patch().contains(receipt.wire.image.as_str()));
        assert!(candidate.description().contains("not run"));
        assert!(candidate.description().contains(&receipt.wire.digest()));
        assert!(candidate.description().contains("--unidiff-zero"));
        assert!(
            candidate
                .description()
                .contains("clean checkout at the exact base")
        );
        // Then the operator can locate the incident and cite evidence without exporting its payload.
        let handoff: serde_json::Value = serde_json::from_str(&candidate.json()).unwrap();
        assert_eq!(
            handoff["title"],
            "fix(utility): restore qualified it-tools image"
        );
        assert_eq!(handoff["incident_path"], "/incidents/incident-1");
        assert_eq!(
            handoff["evidence_citations"][0]["evidence_id"],
            "evidence-0001"
        );
        assert_eq!(handoff["evidence_citations"][0]["source"], "Health");
        assert!(
            handoff["evidence_citations"][0]["content_digest"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        assert!(!candidate.json().contains("fixture health"));
        assert_eq!(store.journal().entries().len(), 3);

        // Given an isolated checkout, never the user's repository or Git credentials.
        let checkout = std::env::temp_dir().join(format!("ai-sre-u9-patch-{}", std::process::id()));
        std::fs::create_dir_all(checkout.join("stacks/utility")).unwrap();
        let target = checkout.join("stacks/utility/compose.yml");
        std::fs::write(&target, source).unwrap();
        // When Git applies the explicitly zero-context patch to the captured source.
        use std::io::Write;
        // A non-final image line needs the documented mode; default Git context checks reject it.
        let mut ordinary = std::process::Command::new("/usr/bin/git")
            .args(["apply", "--check", "-"])
            .env_clear()
            .current_dir(&checkout)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        ordinary
            .stdin
            .take()
            .unwrap()
            .write_all(candidate.patch().as_bytes())
            .unwrap();
        assert!(!ordinary.wait().unwrap().success());
        let mut git = std::process::Command::new("/usr/bin/git")
            .args(["apply", "--unidiff-zero", "-"])
            .env_clear()
            .current_dir(&checkout)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        git.stdin
            .take()
            .unwrap()
            .write_all(candidate.patch().as_bytes())
            .unwrap();
        assert!(git.wait().unwrap().success());
        // Then the resulting file is byte-for-byte the deterministic renderer output.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            crate::gitops::it_tools_image::render(source, &receipt.wire.image).unwrap()
        );

        // Given an immutable committed source and its journaled candidate for offline validation.
        std::fs::write(&target, source).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("/usr/bin/git")
                .args(args)
                .env_clear()
                .current_dir(&checkout)
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        };
        git(&["init", "-b", "main"]);
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "fixture",
        ]);
        let base = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
        let request = ManualRepairRequest {
            expected_base: base.trim(),
            current_base: base.trim(),
            ..request
        };
        let candidate = store.prepare_manual_candidate(&request).unwrap().unwrap();
        use crate::gitops::{
            sandbox::{SandboxLimits, SandboxPlan},
            snapshot::RepositorySnapshot,
        };
        let plan = SandboxPlan::new(
            &format!(
                "ghcr.io/bodhispace-xyz/ai-sre-validator@sha256:{}",
                "c".repeat(64)
            ),
            SandboxLimits::default(),
        )
        .unwrap();
        let wrong_image = format!("ghcr.io/corentinth/it-tools@sha256:{}", "f".repeat(64));
        let wrong = RepositorySnapshot::capture(&checkout, base.trim(), &wrong_image)
            .await
            .unwrap();
        // When the snapshot contains a different repair than the journaled candidate.
        let rejected = plan.prepare("u9-wrong", &candidate, wrong);
        // Then it cannot become the candidate's validation input, even with the same base SHA.
        assert!(rejected.is_err());

        // Given the exact qualified replacement applied to the committed source.
        let snapshot = RepositorySnapshot::capture(&checkout, base.trim(), candidate.image())
            .await
            .unwrap();
        let digest = snapshot.digest().to_owned();
        // When the sandbox plan takes ownership of the matching snapshot.
        let prepared = plan.prepare("u9-matching", &candidate, snapshot).unwrap();
        // Then candidate and source identities remain bound without claiming validation ran.
        assert_eq!(prepared.artifact_digest(), candidate.artifact_digest());
        assert_eq!(prepared.snapshot_digest(), digest);
        assert!(
            candidate
                .description()
                .contains("Sandbox validation: not run")
        );
        let mount = prepared
            .arguments()
            .iter()
            .find_map(|arg| arg.strip_prefix("type=bind,src="))
            .unwrap();
        let directory =
            std::path::PathBuf::from(mount.strip_suffix(",dst=/source,ro=true").unwrap());
        assert!(directory.join("stacks/utility/compose.yml").is_file());
        assert!(!directory.join(".git").exists());
        use std::os::unix::fs::PermissionsExt;
        for relative in ["", "stacks", "stacks/utility"] {
            assert_eq!(
                std::fs::metadata(directory.join(relative))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
        assert_eq!(
            std::fs::metadata(directory.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        drop(prepared);
        assert!(!directory.exists());
        // Given the exact operator procedure from the runbook and the actual generated patch.
        let runbook = include_str!("../../../packaging/validator/OPERATOR.md");
        let procedure = runbook
            .split("<!-- begin guarded patch procedure -->")
            .nth(1)
            .unwrap()
            .split("```sh\n")
            .nth(1)
            .unwrap()
            .split("```")
            .next()
            .unwrap();
        let patch_path = checkout.with_extension("patch");
        std::fs::write(&patch_path, candidate.patch()).unwrap();
        let apply = |at: &std::path::Path, expected_base: &str| {
            std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    &format!("{procedure}\napply_reviewed_patch \"$1\" \"$2\""),
                    "operator-procedure",
                    expected_base,
                ])
                .arg(&patch_path)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .current_dir(at)
                .output()
                .unwrap()
                .status
                .success()
        };
        // When base identity, repository root, or clean-checkout preconditions are violated.
        assert!(!apply(&checkout, &"f".repeat(40)));
        assert!(!apply(&checkout.join("stacks"), base.trim()));
        let dirty = source.replace("unless-stopped", "always");
        std::fs::write(&target, &dirty).unwrap();
        assert!(!apply(&checkout, base.trim()));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), dirty);
        std::fs::write(&target, source).unwrap();
        std::fs::write(checkout.join("untracked"), "operator work").unwrap();
        assert!(!apply(&checkout, base.trim()));
        std::fs::remove_file(checkout.join("untracked")).unwrap();
        // Then only the clean exact-base checkout is changed, and its staged bytes match validation.
        assert!(apply(&checkout, base.trim()));
        let expected = crate::gitops::it_tools_image::render(source, candidate.image()).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), expected);
        assert_eq!(
            String::from_utf8(git(&["show", ":stacks/utility/compose.yml"])).unwrap(),
            expected
        );
        assert_eq!(
            String::from_utf8(git(&["diff", "--cached", "--name-only"])).unwrap(),
            "stacks/utility/compose.yml\n"
        );
        std::fs::remove_file(patch_path).unwrap();
        std::fs::remove_dir_all(checkout).unwrap();
    }

    #[test]
    fn persisted_qualification_requires_its_matching_journal_fact() {
        // Given a legitimate receipt whose database projection was altered independently.
        let path = std::env::temp_dir().join(format!(
            "ai-sre-u9-divergence-{}.sqlite",
            std::process::id()
        ));
        let receipt = fixture();
        let mut store = JournalStore::open(&path).unwrap();
        store.record_deployment(&receipt, 221).unwrap();
        // When a restore/corruption leaves the table without the corresponding event.
        store
            .connection
            .execute("DELETE FROM journal_events", [])
            .unwrap();
        drop(store);
        let mut restored = JournalStore::open(&path).unwrap();
        // Then the projection cannot independently grant a qualified rollback source.
        assert!(
            restored
                .qualified_deployment(receipt.deployment_id(), 222)
                .is_err()
        );
        assert!(restored.record_deployment(&receipt, 222).is_err());
        drop(restored);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn qualified_deployment_survives_reopen_but_contradiction_is_permanent() {
        // Given a protected completion receipt covering a predeclared healthy window.
        let path =
            std::env::temp_dir().join(format!("ai-sre-u9-qualified-{}.sqlite", std::process::id()));
        let receipt = fixture();
        let mut store = JournalStore::open(&path).unwrap();
        // When qualification is committed, replayed, and reopened from real SQLite.
        assert!(store.record_deployment(&receipt, 221).unwrap());
        assert!(store.record_deployment(&receipt, 221).unwrap());
        assert_eq!(store.journal().entries().len(), 1);
        drop(store);
        let mut reopened = JournalStore::open(&path).unwrap();
        assert!(
            reopened
                .qualified_deployment(receipt.deployment_id(), 222)
                .unwrap()
                .is_some()
        );
        // Then contradictory evidence invalidates that identity even after the old receipt is replayed.
        let mut contradiction = fixture();
        contradiction.wire.observed_image =
            format!("ghcr.io/corentinth/it-tools@sha256:{}", "c".repeat(64));
        assert!(!reopened.record_deployment(&contradiction, 222).unwrap());
        // The rejected protected receipt remains reconstructable after its inbox file disappears.
        let retained: String = reopened
            .connection
            .query_row(
                "SELECT receipt_json FROM deployment_receipt_audit WHERE receipt_digest=?1",
                [contradiction.wire.digest()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            retained,
            serde_json::to_string(&contradiction.wire).unwrap()
        );
        assert!(matches!(reopened.journal().entries().last().map(|entry| &entry.event), Some(crate::reasoning::journal::JournalEvent::DeploymentQualification { reason: Some(crate::reasoning::journal::DeploymentQualificationReason::ContradictoryReceipt), eligible: false, .. })));
        assert!(!reopened.record_deployment(&receipt, 223).unwrap());
        assert!(
            reopened
                .qualified_deployment(receipt.deployment_id(), 223)
                .unwrap()
                .is_none()
        );
        drop(reopened);
        std::fs::remove_file(path).unwrap();
    }
}
