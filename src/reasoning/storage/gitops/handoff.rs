//! Atomically commits validated operator artifacts after rechecking their durable authority inputs.

use super::{JournalStore, JournalStoreError, evidence_binding};
use crate::{
    gitops::handoff::{ManualHandoffRequest, ManualRepairHandoff},
    reasoning::journal::{JournalEntry, JournalEvent},
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

impl JournalStore {
    /// Rechecks qualification, scoped evidence, candidate provenance, base, and validation freshness.
    /// Returns no artifact when any input is stale or mismatched. It never publishes a PR.
    pub fn finalize_manual_repair(
        &mut self,
        request: &ManualHandoffRequest<'_>,
    ) -> Result<Option<ManualRepairHandoff>, JournalStoreError> {
        let Some(handoff) = ManualRepairHandoff::build(request) else {
            return Ok(None);
        };
        let candidate = request.candidate;
        let deployment: Option<String> = self.connection.query_row(
            "SELECT deployment_id FROM qualified_deployments WHERE receipt_digest=?1 AND eligible=1",
            [candidate.qualification_digest()], |row| row.get(0),
        ).optional()?;
        let Some(deployment) = deployment else {
            return Ok(None);
        };
        let Some(qualified) = self.qualified_deployment(&deployment, request.now)? else {
            return Ok(None);
        };
        if qualified.receipt_digest() != candidate.qualification_digest() {
            return Ok(None);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let still_qualified: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM qualified_deployments WHERE deployment_id=?1 AND receipt_digest=?2 AND eligible=1)",
            params![deployment, candidate.qualification_digest()], |row| row.get(0),
        )?;
        if !still_qualified
            || evidence_binding(&tx, request.context)?
                .map(|binding| binding.digest)
                .as_deref()
                != Some(candidate.evidence_digest())
        {
            return Ok(None);
        }
        let candidate_digest = candidate.artifact_digest();
        let stored_candidate: Option<String> = tx
            .query_row(
                "SELECT candidate_json FROM manual_repair_candidates WHERE artifact_digest=?1",
                [&candidate_digest],
                |row| row.get(0),
            )
            .optional()?;
        let candidate_fact: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM journal_events WHERE incident_id=?1 AND run_id=?2 AND json_extract(event_json,'$.ManualRepairPrepared.artifact_digest')=?3 AND json_extract(event_json,'$.ManualRepairPrepared.qualification_digest')=?4)",
            params![request.context.incident_id, request.context.run_id, candidate_digest, candidate.qualification_digest()],
            |row| row.get(0),
        )?;
        if stored_candidate.as_deref() != Some(candidate.json().as_str()) || !candidate_fact {
            return Ok(None);
        }
        let handoff_digest = handoff.digest();
        let existing: Option<String> = tx
            .query_row(
                "SELECT handoff_json FROM manual_repair_handoffs WHERE handoff_digest=?1",
                [&handoff_digest],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            let recorded: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM journal_events WHERE incident_id=?1 AND run_id=?2 AND json_extract(event_json,'$.ManualRepairValidated.handoff_digest')=?3 AND json_extract(event_json,'$.ManualRepairValidated.artifact_digest')=?4)",
                params![request.context.incident_id, request.context.run_id, handoff_digest, candidate_digest],
                |row| row.get(0),
            )?;
            if !recorded || existing != handoff.json() {
                return Err(JournalStoreError::InvalidDeployment);
            }
            return Ok(Some(handoff));
        }
        let sequence = self.journal.entries().len() as u64;
        let sql_sequence =
            i64::try_from(sequence).map_err(|_| JournalStoreError::NonContiguousSequence)?;
        let count: i64 =
            tx.query_row("SELECT COUNT(*) FROM journal_events", [], |row| row.get(0))?;
        if count != sql_sequence {
            return Err(JournalStoreError::NonContiguousSequence);
        }
        let event = JournalEvent::ManualRepairValidated {
            handoff_digest: handoff_digest.clone(),
            artifact_digest: candidate_digest,
            at_unix_seconds: request.now,
        };
        tx.execute(
            "INSERT INTO manual_repair_handoffs(handoff_digest,handoff_json) VALUES (?1,?2)",
            params![handoff_digest, handoff.json()],
        )?;
        tx.execute("INSERT INTO journal_events(sequence,event_json,incident_id,run_id) VALUES (?1,?2,?3,?4)",
            params![sql_sequence, serde_json::to_string(&event)?, request.context.incident_id, request.context.run_id])?;
        tx.commit()?;
        self.journal.restore(JournalEntry {
            sequence,
            context: Some(request.context.clone()),
            event,
        });
        Ok(Some(handoff))
    }
}
