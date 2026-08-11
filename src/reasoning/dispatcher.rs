//! Single-writer incident dispatch and durable deduplication.
//!
//! The dispatcher owns queue consumption, incident identity replay, and
//! lifecycle facts. It does not authorize or execute mutations.

use std::collections::BTreeSet;

use thiserror::Error;

use crate::transport::IntakeBatch;

use super::{
    incident::AlertStatus,
    journal::JournalEvent,
    storage::{JournalStore, JournalStoreError},
};

/// Result of processing one normalized incident signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// The signal started or advanced a new incident workflow.
    Accepted,
    /// The signal was already represented and was not duplicated.
    Deduplicated,
}

/// Errors while recording an accepted webhook batch.
#[derive(Debug, Error)]
pub enum DispatchError {
    /// The durable lifecycle fact could not be written.
    #[error("incident dispatch journal write failed")]
    Journal(#[from] JournalStoreError),
}

/// Durable single-writer dispatcher reconstructed from prior journal facts.
#[derive(Debug)]
pub struct IncidentDispatcher {
    journal: JournalStore,
    seen_incidents: BTreeSet<String>,
}

impl IncidentDispatcher {
    /// Rebuilds deduplication state from the durable journal.
    pub fn new(journal: JournalStore) -> Self {
        let seen_incidents = journal.journal().incident_ids();
        Self {
            journal,
            seen_incidents,
        }
    }

    /// Processes one queue batch in input order.
    pub fn process(&mut self, batch: IntakeBatch) -> Result<Vec<DispatchOutcome>, DispatchError> {
        batch
            .incidents
            .into_iter()
            .map(|incident| {
                if self.seen_incidents.contains(&incident.incident_id) {
                    self.journal.append(JournalEvent::AlertDeduplicated {
                        incident_id: incident.incident_id,
                    })?;
                    return Ok(DispatchOutcome::Deduplicated);
                }
                let event = match incident.status {
                    AlertStatus::Firing => JournalEvent::IncidentOpened {
                        incident_id: incident.incident_id.clone(),
                        alert_name: incident.alert_name,
                    },
                    AlertStatus::Resolved => JournalEvent::IncidentRecovered {
                        incident_id: incident.incident_id.clone(),
                    },
                };
                self.journal.append(event)?;
                self.seen_incidents.insert(incident.incident_id);
                Ok(DispatchOutcome::Accepted)
            })
            .collect()
    }

    /// Returns the durable journal for replay and later investigation wiring.
    pub fn journal(&self) -> &JournalStore {
        &self.journal
    }
}
