//! Persists responder-side validation attempts before network dispatch, preventing silent re-execution.

use super::{JournalStore, JournalStoreError};
use crate::reasoning::journal::{
    JournalContext, JournalEntry, JournalEvent, ManualValidationStage,
};
use rusqlite::{OptionalExtension, params};

/// Durable attempt status; neither state grants publication or deployment authority.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub enum ManualValidationState {
    /// Dispatch was reserved; the remote outcome may be unknown, including after a crash.
    Uncertain,
    /// An authenticated bound receipt was received; handoff still requires current journal checks.
    Validated,
}

/// Recovery information for one candidate's validation attempt, not a reusable validation receipt.
#[derive(serde::Serialize)]
pub struct ManualValidationAttempt {
    /// Whether a bound response has been durably received.
    pub state: ManualValidationState,
    /// Exact originally reserved request, including its nonce and expiry.
    pub request_json: String,
    /// Serialized received receipt, for audit only; this does not mint the sealed receipt type.
    pub receipt_json: Option<String>,
}

impl ManualValidationAttempt {
    /// Historical worker cleanup evidence; never permission to dispatch, publish, or deploy.
    pub fn cleanup_status(&self) -> &'static str {
        if self.receipt_json.is_some() {
            "confirmed_by_receipt"
        } else {
            "unknown"
        }
    }
}

/// Stored review data, never evidence of current readiness or permission to publish.
#[derive(serde::Serialize)]
pub struct ManualRepairArtifact {
    /// Original candidate JSON containing the patch and proposed PR description.
    pub candidate_json: String,
    /// Latest reserved validation attempt, if validation has been requested.
    pub attempt: Option<ManualValidationAttempt>,
    /// Most recently inserted handoff, for historical review only; no current readiness is implied.
    pub handoff_json: Option<String>,
}

/// Append-only recovery audit containing the previous attempt and kernel-authenticated operator.
#[derive(serde::Serialize)]
pub struct ManualValidationRecovery {
    /// Archived request, retained even when its remote outcome was uncertain.
    pub request_json: String,
    /// Receipt retained for audit only, if it arrived before recovery.
    pub receipt_json: Option<String>,
    /// Effective user identity reported by the local socket's kernel credentials.
    pub operator_uid: u32,
    /// Effective primary group reported by the local socket's kernel credentials.
    pub operator_gid: u32,
    /// Operator explanation; callers must not include credentials or raw evidence.
    pub reason: String,
    /// Server-observed Unix second at recovery.
    pub recovered_at: i64,
}

impl JournalStore {
    /// Records an observed transport/validation error without changing the uncertain reservation.
    pub(crate) fn record_manual_validation_failure(
        &mut self,
        artifact: &str,
        wire: &str,
        elapsed: std::time::Duration,
    ) -> Result<(), JournalStoreError> {
        let elapsed =
            u64::try_from(elapsed.as_millis()).map_err(|_| JournalStoreError::InvalidDeployment)?;
        let request_digest = crate::gitops::receipt::digest(wire.as_bytes());
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM manual_validation_attempts WHERE artifact_digest=?1 AND request_json=?2 AND receipt_json IS NULL)", params![artifact, wire], |r| r.get(0))?;
        if !active {
            return Err(JournalStoreError::InvalidDeployment);
        }
        let recorded: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM journal_events WHERE json_extract(event_json,'$.ManualValidation.artifact_digest')=?1 AND json_extract(event_json,'$.ManualValidation.request_digest')=?2 AND json_extract(event_json,'$.ManualValidation.stage')='ResponseFailed')", params![artifact, request_digest], |r| r.get(0))?;
        if recorded {
            return Ok(());
        }
        let entry = record_transition(
            &tx,
            self.journal.entries().len(),
            artifact,
            &request_digest,
            ManualValidationStage::ResponseFailed,
            None,
            Some(elapsed),
        )?;
        tx.commit()?;
        self.journal.restore(entry);
        Ok(())
    }

    /// Returns the most recent 100 recovery records; older records remain in SQLite.
    pub fn manual_validation_history(
        &self,
        artifact: &str,
    ) -> Result<Vec<ManualValidationRecovery>, JournalStoreError> {
        let mut statement = self.connection.prepare("SELECT request_json,receipt_json,operator_uid,operator_gid,reason,recovered_at FROM manual_validation_recoveries WHERE artifact_digest=?1 ORDER BY sequence DESC LIMIT 100")?;
        Ok(statement
            .query_map([artifact], |row| {
                Ok(ManualValidationRecovery {
                    request_json: row.get(0)?,
                    receipt_json: row.get(1)?,
                    operator_uid: row.get(2)?,
                    operator_gid: row.get(3)?,
                    reason: row.get(4)?,
                    recovered_at: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    // Only the local authenticated admin adapter may supply these identity fields. Recovery
    // archives the old attempt atomically; it neither claims remote cleanup nor runs a new job.
    pub(crate) fn recover_manual_validation(
        &mut self,
        artifact: &str,
        expected: &str,
        uid: u32,
        gid: u32,
        reason: &str,
        now: u64,
    ) -> Result<bool, JournalStoreError> {
        if reason.trim().is_empty() || reason.len() > 512 || reason.chars().any(char::is_control) {
            return Err(JournalStoreError::InvalidDeployment);
        }
        let stored_now = i64::try_from(now).map_err(|_| JournalStoreError::InvalidDeployment)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let previous: Option<(String, Option<String>)> = transaction.query_row("SELECT request_json,receipt_json FROM manual_validation_attempts WHERE artifact_digest=?1", [artifact], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        let Some((wire, receipt)) = previous else {
            return Ok(false);
        };
        let request: serde_json::Value = serde_json::from_str(&wire)?;
        if crate::gitops::receipt::digest(wire.as_bytes()) != expected
            || request
                .get("expires_at")
                .and_then(serde_json::Value::as_u64)
                .is_none_or(|expiry| now <= expiry)
        {
            return Ok(false);
        }
        transaction.execute("INSERT INTO manual_validation_recoveries(artifact_digest,request_digest,request_json,receipt_json,operator_uid,operator_gid,reason,recovered_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)", params![artifact, expected, wire, receipt, uid, gid, reason, stored_now])?;
        transaction.execute(
            "DELETE FROM manual_validation_attempts WHERE artifact_digest=?1",
            [artifact],
        )?;
        let entry = record_transition(
            &transaction,
            self.journal.entries().len(),
            artifact,
            expected,
            ManualValidationStage::RecoveryAuthorized,
            Some(now),
            None,
        )?;
        transaction.commit()?;
        self.journal.restore(entry);
        Ok(true)
    }

    /// Retrieves review data in one SQLite snapshot without dispatching work or restoring authority.
    pub fn manual_repair_artifact(
        &self,
        artifact: &str,
    ) -> Result<Option<ManualRepairArtifact>, JournalStoreError> {
        Ok(self.connection.query_row(
            "SELECT c.candidate_json,a.request_json,a.receipt_json,(SELECT h.handoff_json FROM manual_repair_handoffs h WHERE json_extract(h.handoff_json,'$.candidate_digest')=c.artifact_digest ORDER BY h.rowid DESC LIMIT 1) FROM manual_repair_candidates c LEFT JOIN manual_validation_attempts a ON a.artifact_digest=c.artifact_digest WHERE c.artifact_digest=?1",
            [artifact],
            |row| {
                let request_json: Option<String> = row.get(1)?;
                let receipt_json: Option<String> = row.get(2)?;
                Ok(ManualRepairArtifact {
                    candidate_json: row.get(0)?,
                    handoff_json: row.get(3)?,
                    attempt: request_json.map(|request_json| ManualValidationAttempt {
                        state: if receipt_json.is_some() { ManualValidationState::Validated } else { ManualValidationState::Uncertain },
                        request_json,
                        receipt_json,
                    }),
                })
            },
        ).optional()?)
    }

    pub(crate) fn reserve_manual_validation(
        &mut self,
        artifact: &str,
        request_json: &str,
    ) -> Result<bool, JournalStoreError> {
        if request_json.len() > 16 * 1024 {
            return Err(JournalStoreError::InvalidDeployment);
        }
        let request = serde_json::from_str::<serde_json::Value>(request_json)?;
        let request_digest = crate::gitops::receipt::digest(request_json.as_bytes());
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let count = tx.execute("INSERT INTO manual_validation_attempts(artifact_digest,request_json) SELECT ?1,?2 WHERE NOT EXISTS (SELECT 1 FROM manual_validation_recoveries WHERE artifact_digest=?1 AND request_digest=?3) ON CONFLICT(artifact_digest) DO NOTHING", params![artifact, request_json, request_digest])?;
        if count != 1 {
            return Ok(false);
        }
        let entry = record_transition(
            &tx,
            self.journal.entries().len(),
            artifact,
            &request_digest,
            ManualValidationStage::Reserved,
            request.get("issued_at").and_then(serde_json::Value::as_u64),
            None,
        )?;
        tx.commit()?;
        self.journal.restore(entry);
        Ok(true)
    }

    pub(crate) fn complete_manual_validation(
        &mut self,
        receipt: &crate::gitops::sandbox::ValidationReceipt,
        request_json: &str,
        response_elapsed: Option<std::time::Duration>,
    ) -> Result<(), JournalStoreError> {
        let response_elapsed_ms = response_elapsed
            .map(|elapsed| u64::try_from(elapsed.as_millis()))
            .transpose()
            .map_err(|_| JournalStoreError::InvalidDeployment)?;
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let count = tx.execute("UPDATE manual_validation_attempts SET receipt_json=?1 WHERE artifact_digest=?2 AND request_json=?3 AND receipt_json IS NULL", params![serde_json::to_string(receipt)?, receipt.artifact_digest(), request_json])?;
        if count != 1 {
            return Err(JournalStoreError::InvalidDeployment);
        }
        let entry = record_transition(
            &tx,
            self.journal.entries().len(),
            receipt.artifact_digest(),
            &crate::gitops::receipt::digest(request_json.as_bytes()),
            ManualValidationStage::ReceiptReceived,
            Some(receipt.completed_at()),
            response_elapsed_ms,
        )?;
        tx.commit()?;
        self.journal.restore(entry);
        Ok(())
    }

    /// Reads durable attempt state without rerunning work or importing a receipt as authority.
    pub fn manual_validation_attempt(
        &self,
        artifact: &str,
    ) -> Result<Option<ManualValidationAttempt>, JournalStoreError> {
        Ok(self.connection.query_row("SELECT request_json,receipt_json FROM manual_validation_attempts WHERE artifact_digest=?1", [artifact], |row| {
            let receipt_json: Option<String> = row.get(1)?;
            Ok(ManualValidationAttempt { state: if receipt_json.is_some() { ManualValidationState::Validated } else { ManualValidationState::Uncertain }, request_json: row.get(0)?, receipt_json })
        }).optional()?)
    }
}

// The caller owns the transaction and updates the in-memory projection only after commit.
// Legacy attempts without a prepared scope remain unscoped; no incident identity is guessed.
fn record_transition(
    tx: &rusqlite::Transaction<'_>,
    sequence: usize,
    artifact: &str,
    request_digest: &str,
    stage: ManualValidationStage,
    at_unix_seconds: Option<u64>,
    response_elapsed_ms: Option<u64>,
) -> Result<JournalEntry, JournalStoreError> {
    let sql_sequence =
        i64::try_from(sequence).map_err(|_| JournalStoreError::NonContiguousSequence)?;
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM journal_events", [], |row| row.get(0))?;
    if count != sql_sequence {
        return Err(JournalStoreError::NonContiguousSequence);
    }
    let mut statement = tx.prepare("SELECT DISTINCT incident_id,run_id FROM journal_events WHERE json_extract(event_json,'$.ManualRepairPrepared.artifact_digest')=?1 LIMIT 2")?;
    let scopes = statement
        .query_map([artifact], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let context = match scopes.as_slice() {
        [] | [(None, None)] => None,
        [(Some(incident_id), Some(run_id))] => Some(JournalContext {
            incident_id: incident_id.clone(),
            run_id: run_id.clone(),
        }),
        _ => return Err(JournalStoreError::InvalidDeployment),
    };
    let event = JournalEvent::ManualValidation {
        artifact_digest: artifact.into(),
        request_digest: request_digest.into(),
        stage,
        at_unix_seconds,
        response_elapsed_ms,
    };
    tx.execute(
        "INSERT INTO journal_events(sequence,event_json,incident_id,run_id) VALUES (?1,?2,?3,?4)",
        params![
            sql_sequence,
            serde_json::to_string(&event)?,
            context.as_ref().map(|c| &c.incident_id),
            context.as_ref().map(|c| &c.run_id)
        ],
    )?;
    Ok(JournalEntry {
        sequence: sequence as u64,
        context,
        event,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_response_is_timed_once_but_never_clears_uncertain_attempt() {
        // Given a request whose remote outcome cannot be established after a transport error.
        let mut journal = JournalStore::open(":memory:").unwrap();
        let wire = r#"{"nonce":"one","expires_at":100}"#;
        journal
            .reserve_manual_validation("candidate", wire)
            .unwrap();
        journal.connection.execute_batch("CREATE TRIGGER reject_failure BEFORE INSERT ON journal_events BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
        assert!(
            journal
                .record_manual_validation_failure(
                    "candidate",
                    wire,
                    std::time::Duration::from_millis(17)
                )
                .is_err()
        );
        assert_eq!(
            journal
                .journal()
                .project()
                .manual_validation_failed_responses,
            0
        );
        journal
            .connection
            .execute_batch("DROP TRIGGER reject_failure;")
            .unwrap();
        // When the responder records its measured wait and receives a duplicate error callback.
        for _ in 0..2 {
            journal
                .record_manual_validation_failure(
                    "candidate",
                    wire,
                    std::time::Duration::from_millis(17),
                )
                .unwrap();
        }
        // Then replay counts one failed wait, but does not claim cleanup or permit another dispatch.
        let progress = journal.journal().project();
        assert_eq!(progress.manual_validation_failed_responses, 1);
        assert_eq!(progress.manual_validation_failed_response_ms, 17);
        assert_eq!(progress.manual_validation_timed_failed_responses, 1);
        let metrics = crate::observability::render_journal_metrics(journal.journal());
        assert!(metrics.contains("ai_sre_manual_validation_failed_responses_total 1\n"));
        assert!(metrics.contains("ai_sre_manual_validation_cleanup_confirmations_total 0\n"));
        assert_eq!(
            journal
                .manual_validation_attempt("candidate")
                .unwrap()
                .unwrap()
                .state,
            ManualValidationState::Uncertain
        );
        assert!(
            !journal
                .reserve_manual_validation("candidate", r#"{"nonce":"two"}"#)
                .unwrap()
        );
    }

    #[test]
    fn journal_failure_rolls_back_reservation_and_recovery() {
        // Given a journal whose event table rejects writes at the SQLite boundary.
        let mut journal = JournalStore::open(":memory:").unwrap();
        journal.connection.execute_batch("CREATE TRIGGER reject_fact BEFORE INSERT ON journal_events BEGIN SELECT RAISE(ABORT, 'injected journal failure'); END;").unwrap();
        let wire = r#"{"nonce":"one","expires_at":100}"#;
        // When reservation cannot commit its audit fact.
        assert!(
            journal
                .reserve_manual_validation("candidate", wire)
                .is_err()
        );
        // Then no active attempt survives and the in-memory journal stays unchanged.
        assert!(
            journal
                .manual_validation_attempt("candidate")
                .unwrap()
                .is_none()
        );
        assert!(journal.journal().entries().is_empty());
        journal
            .connection
            .execute_batch("DROP TRIGGER reject_fact;")
            .unwrap();
        assert!(
            journal
                .reserve_manual_validation("candidate", wire)
                .unwrap()
        );
        journal.connection.execute_batch("CREATE TRIGGER reject_fact BEFORE INSERT ON journal_events BEGIN SELECT RAISE(ABORT, 'injected journal failure'); END;").unwrap();

        // When recovery cannot commit its fact, the original reservation must remain recoverable.
        let digest = crate::gitops::receipt::digest(wire.as_bytes());
        assert!(
            journal
                .recover_manual_validation("candidate", &digest, 501, 20, "checked", 101)
                .is_err()
        );
        // Then the archive and deletion both roll back, without changing the replay projection.
        assert!(
            journal
                .manual_validation_history("candidate")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            journal
                .manual_validation_attempt("candidate")
                .unwrap()
                .unwrap()
                .request_json,
            wire
        );
        assert_eq!(journal.journal().project().manual_validation_recoveries, 0);
    }

    #[test]
    fn ambiguous_candidate_scope_cannot_commit_a_reservation() {
        // Given contradictory preparation facts claiming the same artifact for different runs.
        let mut journal = JournalStore::open(":memory:").unwrap();
        for run in ["one", "two"] {
            journal
                .append_scoped(
                    JournalEvent::ManualRepairPrepared {
                        artifact_digest: "candidate".into(),
                        qualification_digest: "qualification".into(),
                        at_unix_seconds: 1,
                    },
                    Some(&JournalContext {
                        incident_id: "incident".into(),
                        run_id: run.into(),
                    }),
                )
                .unwrap();
        }
        // When the store cannot assign validation to one unambiguous scope.
        assert!(
            journal
                .reserve_manual_validation("candidate", r#"{"nonce":"one"}"#)
                .is_err()
        );
        // Then it fails closed without retaining an unaudited reservation or guessing a run.
        assert!(
            journal
                .manual_validation_attempt("candidate")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            journal.journal().project().manual_validation_reservations,
            0
        );
    }

    #[test]
    fn recovery_preserves_original_attempt_and_rejects_replayed_requests() {
        // Given an expired, uncertain dispatch and an operator looking at that exact request.
        let mut journal = JournalStore::open(":memory:").unwrap();
        let wire = r#"{"nonce":"old","expires_at":100}"#;
        journal
            .reserve_manual_validation("candidate", wire)
            .unwrap();
        let expected = crate::gitops::receipt::digest(wire.as_bytes());
        // When the authenticated operator permits revalidation after the request deadline.
        assert!(
            !journal
                .recover_manual_validation("candidate", &expected, 501, 20, "worker inspected", 100)
                .unwrap()
        );
        assert!(
            journal
                .recover_manual_validation("candidate", &expected, 501, 20, "worker inspected", 101)
                .unwrap()
        );
        // Then history retains the uncertain request and a replay cannot clear a later attempt.
        assert!(
            journal
                .manual_validation_attempt("candidate")
                .unwrap()
                .is_none()
        );
        let history = journal.manual_validation_history("candidate").unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].request_json, wire);
        assert_eq!(history[0].operator_uid, 501);
        assert!(history[0].receipt_json.is_none());
        let progress = journal.journal().project();
        assert_eq!(progress.manual_validation_reservations, 1);
        assert_eq!(progress.manual_validation_recoveries, 1);
        assert_eq!(progress.manual_validation_receipts, 0);
        let metrics = crate::observability::render_journal_metrics(journal.journal());
        assert!(metrics.contains("ai_sre_manual_validation_reservations_total 1\n"));
        assert!(metrics.contains("ai_sre_manual_validation_recoveries_total 1\n"));
        assert!(metrics.contains("ai_sre_manual_validation_receipts_total 0\n"));
        assert!(!metrics.contains("worker inspected"));
        assert!(!metrics.contains(&expected));
        assert!(
            !journal
                .reserve_manual_validation("candidate", wire)
                .unwrap()
        );
        journal
            .reserve_manual_validation("candidate", r#"{"nonce":"new","expires_at":200}"#)
            .unwrap();
        assert!(
            !journal
                .recover_manual_validation("candidate", &expected, 501, 20, "replayed", 300)
                .unwrap()
        );
        assert_eq!(
            journal
                .manual_validation_history("candidate")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            journal.journal().project().manual_validation_reservations,
            2
        );
        assert_eq!(journal.journal().project().manual_validation_recoveries, 1);
    }

    #[test]
    fn artifact_lookup_distinguishes_missing_candidates_from_unvalidated_candidates() {
        // Given a stored candidate that has never been dispatched to a worker.
        let mut journal = JournalStore::open(":memory:").unwrap();
        journal.connection.execute("INSERT INTO manual_repair_candidates VALUES ('candidate', '{\"patch\":\"diff\",\"description\":\"review required\"}')", []).unwrap();
        // When an operator retrieves the artifact and an unknown identifier.
        let artifact = journal
            .manual_repair_artifact("candidate")
            .unwrap()
            .unwrap();
        // Then retrieval returns review data without inventing validation or publication authority.
        assert_eq!(
            artifact.candidate_json,
            r#"{"patch":"diff","description":"review required"}"#
        );
        assert!(artifact.attempt.is_none());
        assert!(journal.manual_repair_artifact("missing").unwrap().is_none());
        assert!(
            journal
                .reserve_manual_validation("candidate", r#"{"nonce":"first"}"#)
                .unwrap()
        );
        assert_eq!(
            journal
                .manual_repair_artifact("candidate")
                .unwrap()
                .unwrap()
                .attempt
                .unwrap()
                .state,
            ManualValidationState::Uncertain
        );
    }

    #[test]
    fn uncertain_attempt_survives_reopen_and_rejects_a_new_nonce() {
        // Given a request durably reserved before any network work starts.
        let path = std::env::temp_dir().join(format!("u9-attempt-{}.sqlite", std::process::id()));
        let mut journal = JournalStore::open(&path).unwrap();
        let context = JournalContext {
            incident_id: "incident".into(),
            run_id: "run".into(),
        };
        journal
            .append_scoped(
                JournalEvent::ManualRepairPrepared {
                    artifact_digest: "candidate".into(),
                    qualification_digest: "qualification".into(),
                    at_unix_seconds: 1,
                },
                Some(&context),
            )
            .unwrap();
        let mut stale_owner = JournalStore::open(&path).unwrap();
        assert!(
            journal
                .reserve_manual_validation("candidate", r#"{"nonce":"original","expires_at":100}"#)
                .unwrap()
        );
        // A second owner with an outdated event sequence must not leave an unaudited attempt.
        assert!(matches!(
            stale_owner.reserve_manual_validation("other-candidate", r#"{"nonce":"other"}"#),
            Err(JournalStoreError::NonContiguousSequence)
        ));
        assert!(
            journal
                .manual_validation_attempt("other-candidate")
                .unwrap()
                .is_none()
        );
        drop(stale_owner);
        drop(journal);
        // When a restarted caller creates a fresh nonce for the same candidate.
        let mut journal = JournalStore::open(&path).unwrap();
        assert!(
            !journal
                .reserve_manual_validation("candidate", r#"{"nonce":"replacement"}"#)
                .unwrap()
        );
        // Then the original uncertain request remains intact for explicit recovery.
        let attempt = journal
            .manual_validation_attempt("candidate")
            .unwrap()
            .unwrap();
        assert_eq!(attempt.state, ManualValidationState::Uncertain);
        assert_eq!(
            attempt.request_json,
            r#"{"nonce":"original","expires_at":100}"#
        );
        assert!(attempt.receipt_json.is_none());
        assert_eq!(
            journal
                .efficiency_projection(&context)
                .manual_validation_reservations,
            1
        );
        assert_eq!(
            journal
                .efficiency_projection(&JournalContext {
                    run_id: "other".into(),
                    ..context.clone()
                })
                .manual_validation_reservations,
            0
        );
        let expected = crate::gitops::receipt::digest(attempt.request_json.as_bytes());
        assert!(
            journal
                .recover_manual_validation("candidate", &expected, 501, 20, "worker inspected", 101)
                .unwrap()
        );
        let before = journal.efficiency_projection(&context);
        drop(journal);
        let journal = JournalStore::open(&path).unwrap();
        assert_eq!(journal.efficiency_projection(&context), before);
        assert_eq!(before.manual_validation_recoveries, 1);
        drop(journal);
        std::fs::remove_file(path).unwrap();
    }
}
