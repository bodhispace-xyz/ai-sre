//! Records authenticated operator pickup separately from artifact reads, approval, and deployment.

use super::{JournalStore, JournalStoreError};
use crate::reasoning::journal::{JournalContext, JournalEntry, JournalEvent};
use rusqlite::{OptionalExtension, params};

impl JournalStore {
    /// Returns the historical pickup fact for an exact handoff, without changing readiness.
    pub fn manual_handoff_acknowledgement(
        &self,
        handoff: &str,
    ) -> Result<Option<JournalEvent>, JournalStoreError> {
        let fact: Option<String> = self.connection.query_row("SELECT event_json FROM journal_events WHERE json_extract(event_json,'$.ManualHandoffAcknowledged.handoff_digest')=?1", [handoff], |row| row.get(0)).optional()?;
        fact.map(|fact| serde_json::from_str(&fact).map_err(JournalStoreError::from))
            .transpose()
    }

    /// Only the authenticated local admin adapter supplies identity and server time.
    /// Acknowledging a historical artifact never changes its readiness or dispatches work.
    pub(crate) fn acknowledge_manual_handoff(
        &mut self,
        handoff: &str,
        uid: u32,
        gid: u32,
        now: u64,
    ) -> Result<bool, JournalStoreError> {
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let stored: Option<String> = tx
            .query_row(
                "SELECT handoff_json FROM manual_repair_handoffs WHERE handoff_digest=?1",
                [handoff],
                |r| r.get(0),
            )
            .optional()?;
        let Some(stored) = stored else {
            return Ok(false);
        };
        if crate::gitops::receipt::digest(stored.as_bytes()) != handoff {
            return Err(JournalStoreError::InvalidDeployment);
        }
        let mut statement = tx.prepare("SELECT event_json,incident_id,run_id FROM journal_events WHERE json_extract(event_json,'$.ManualRepairValidated.handoff_digest')=?1 LIMIT 2")?;
        let facts = statement
            .query_map([handoff], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let [(fact, Some(incident_id), Some(run_id))] = facts.as_slice() else {
            return Err(JournalStoreError::InvalidDeployment);
        };
        let JournalEvent::ManualRepairValidated {
            artifact_digest,
            at_unix_seconds,
            ..
        } = serde_json::from_str(fact)?
        else {
            return Err(JournalStoreError::InvalidDeployment);
        };
        let wire: serde_json::Value = serde_json::from_str(&stored)?;
        if wire["candidate_digest"].as_str() != Some(artifact_digest.as_str())
            || wire["incident_digest"].as_str()
                != Some(crate::gitops::receipt::digest(incident_id.as_bytes()).as_str())
            || wire["run_digest"].as_str()
                != Some(crate::gitops::receipt::digest(run_id.as_bytes()).as_str())
        {
            return Err(JournalStoreError::InvalidDeployment);
        }
        let recorded: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM journal_events WHERE json_extract(event_json,'$.ManualHandoffAcknowledged.handoff_digest')=?1)", [handoff], |r| r.get(0))?;
        if recorded {
            return Ok(true);
        }
        let sequence = self.journal.entries().len() as u64;
        let sql_sequence =
            i64::try_from(sequence).map_err(|_| JournalStoreError::NonContiguousSequence)?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM journal_events", [], |r| r.get(0))?;
        if sql_sequence != count {
            return Err(JournalStoreError::NonContiguousSequence);
        }
        let event = JournalEvent::ManualHandoffAcknowledged {
            handoff_digest: handoff.into(),
            artifact_digest,
            operator_uid: uid,
            operator_gid: gid,
            offered_at_unix_seconds: at_unix_seconds,
            acknowledged_at_unix_seconds: now,
        };
        tx.execute("INSERT INTO journal_events(sequence,event_json,incident_id,run_id) VALUES (?1,?2,?3,?4)", params![sql_sequence, serde_json::to_string(&event)?, incident_id, run_id])?;
        let context = JournalContext {
            incident_id: incident_id.clone(),
            run_id: run_id.clone(),
        };
        drop(statement);
        tx.commit()?;
        self.journal.restore(JournalEntry {
            sequence,
            context: Some(context),
            event,
        });
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contradictory_handoff_correlation_cannot_be_acknowledged() {
        // Given a stored handoff whose journal fact names a different candidate.
        let mut store = JournalStore::open(":memory:").unwrap();
        let wire = serde_json::json!({"candidate_digest":"actual", "incident_digest":crate::gitops::receipt::digest(b"incident"), "run_digest":crate::gitops::receipt::digest(b"run")}).to_string();
        let digest = crate::gitops::receipt::digest(wire.as_bytes());
        store
            .connection
            .execute(
                "INSERT INTO manual_repair_handoffs VALUES (?1,?2)",
                params![digest, wire],
            )
            .unwrap();
        store
            .append_scoped(
                JournalEvent::ManualRepairValidated {
                    handoff_digest: digest.clone(),
                    artifact_digest: "different".into(),
                    at_unix_seconds: 1,
                },
                Some(&JournalContext {
                    incident_id: "incident".into(),
                    run_id: "run".into(),
                }),
            )
            .unwrap();
        // When an authenticated operator requests pickup of that inconsistent historical record.
        assert!(
            store
                .acknowledge_manual_handoff(&digest, 501, 20, 2)
                .is_err()
        );
        // Then no pickup fact or timing is attributed to the wrong candidate.
        assert!(
            store
                .manual_handoff_acknowledgement(&digest)
                .unwrap()
                .is_none()
        );
    }
}
