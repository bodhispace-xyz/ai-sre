//! Single-writer incident dispatch with replayed lifecycle correlation.
//!
//! The dispatcher accepts duplicate signals only when they repeat the current
//! lifecycle state. Resolved episodes close a correlation key, and a later
//! firing creates a new episode rather than suppressing a real recurrence.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::transport::IntakeBatch;

use super::{
    incident::{AlertStatus, IncidentSignal},
    journal::JournalEvent,
    storage::{JournalStore, JournalStoreError, OutboxMessage},
};

/// Result of processing one normalized incident signal.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum LifecycleState {
    Firing {
        episode: u64,
        completed: bool,
        last_event_time: String,
    },
    Resolved {
        episode: u64,
        last_event_time: String,
    },
}

/// Durable single-writer dispatcher reconstructed from lifecycle facts.
#[derive(Debug)]
pub struct IncidentDispatcher {
    journal: JournalStore,
    lifecycle: BTreeMap<String, LifecycleState>,
}

impl IncidentDispatcher {
    /// Rebuilds correlation state from the durable journal.
    pub fn new(journal: JournalStore) -> Self {
        let mut lifecycle = BTreeMap::new();
        for entry in journal.journal().entries() {
            match &entry.event {
                JournalEvent::IncidentOpened {
                    incident_id,
                    event_time,
                    ..
                } => {
                    lifecycle.insert(
                        correlation_key(incident_id),
                        LifecycleState::Firing {
                            episode: episode_number(incident_id),
                            completed: false,
                            last_event_time: event_time.clone(),
                        },
                    );
                }
                JournalEvent::IncidentRecovered {
                    incident_id,
                    event_time,
                } => {
                    lifecycle.insert(
                        correlation_key(incident_id),
                        LifecycleState::Resolved {
                            episode: episode_number(incident_id),
                            last_event_time: event_time.clone(),
                        },
                    );
                }
                JournalEvent::IncidentCompleted { incident_id } => {
                    let key = correlation_key(incident_id);
                    if let Some(LifecycleState::Firing {
                        episode,
                        last_event_time,
                        ..
                    }) = lifecycle.get(&key)
                    {
                        lifecycle.insert(
                            key,
                            LifecycleState::Firing {
                                episode: *episode,
                                completed: true,
                                last_event_time: last_event_time.clone(),
                            },
                        );
                    }
                }
                JournalEvent::IncidentResumed { .. }
                | JournalEvent::AlertDeduplicated { .. }
                | JournalEvent::AlertOutOfOrder { .. }
                | JournalEvent::EvidenceCommitted { .. }
                | JournalEvent::ToolContext { .. }
                | JournalEvent::ToolRequested { .. }
                | JournalEvent::PhaseStarted { .. }
                | JournalEvent::PhaseFinished { .. }
                | JournalEvent::ProviderAttempt(_)
                | JournalEvent::Terminal { .. } => {}
            }
        }
        Self { journal, lifecycle }
    }

    /// Processes one queue batch in input order.
    pub fn process(&mut self, batch: IntakeBatch) -> Result<Vec<DispatchOutcome>, DispatchError> {
        batch
            .incidents
            .into_iter()
            .map(|incident| self.process_one(incident).map(|(_, outcome)| outcome))
            .collect()
    }

    /// Accepts a batch and returns only firing signals that started new work.
    pub fn process_new(
        &mut self,
        batch: IntakeBatch,
    ) -> Result<Vec<IncidentSignal>, DispatchError> {
        let mut accepted = Vec::new();
        for incident in batch.incidents {
            let (incident, outcome) = self.process_one(incident)?;
            if outcome == DispatchOutcome::Accepted && incident.status == AlertStatus::Firing {
                accepted.push(incident);
            }
        }
        Ok(accepted)
    }

    fn process_one(
        &mut self,
        mut incident: IncidentSignal,
    ) -> Result<(IncidentSignal, DispatchOutcome), DispatchError> {
        let key = incident.correlation_id.clone();
        let current = self.lifecycle.get(&key).cloned();
        if let Some(last_event_time) = current.as_ref().map(|state| match state {
            LifecycleState::Firing {
                last_event_time, ..
            }
            | LifecycleState::Resolved {
                last_event_time, ..
            } => last_event_time,
        }) {
            if !incident.event_time.is_empty()
                && !last_event_time.is_empty()
                && incident.event_time.as_str() < last_event_time.as_str()
            {
                self.journal.append(JournalEvent::AlertOutOfOrder {
                    incident_id: incident.incident_id.clone(),
                    status: incident.status,
                    event_time: incident.event_time.clone(),
                })?;
                return Ok((incident, DispatchOutcome::Deduplicated));
            }
        }
        match (incident.status, current) {
            (
                AlertStatus::Firing,
                Some(LifecycleState::Firing {
                    completed: true, ..
                }),
            )
            | (AlertStatus::Resolved, Some(LifecycleState::Resolved { .. })) => {
                self.journal.append(JournalEvent::AlertDeduplicated {
                    incident_id: incident.incident_id.clone(),
                })?;
                Ok((incident, DispatchOutcome::Deduplicated))
            }
            (
                AlertStatus::Firing,
                Some(LifecycleState::Firing {
                    completed: false, ..
                }),
            ) => {
                self.journal.append(JournalEvent::IncidentResumed {
                    incident_id: incident.incident_id.clone(),
                })?;
                Ok((incident, DispatchOutcome::Accepted))
            }
            (AlertStatus::Firing, Some(LifecycleState::Resolved { episode, .. })) => {
                incident.episode = episode + 1;
                incident.incident_id = format!("incident-{}-episode-{}", key, incident.episode);
                self.journal.append(JournalEvent::IncidentOpened {
                    incident_id: incident.incident_id.clone(),
                    alert_name: incident.alert_name.clone(),
                    event_time: incident.event_time.clone(),
                })?;
                self.lifecycle.insert(
                    key,
                    LifecycleState::Firing {
                        episode: incident.episode,
                        completed: false,
                        last_event_time: incident.event_time.clone(),
                    },
                );
                Ok((incident, DispatchOutcome::Accepted))
            }
            (AlertStatus::Resolved, Some(LifecycleState::Firing { .. })) => {
                self.journal.append(JournalEvent::IncidentRecovered {
                    incident_id: incident.incident_id.clone(),
                    event_time: incident.event_time.clone(),
                })?;
                self.lifecycle.insert(
                    key,
                    LifecycleState::Resolved {
                        episode: incident.episode,
                        last_event_time: incident.event_time.clone(),
                    },
                );
                Ok((incident, DispatchOutcome::Accepted))
            }
            (AlertStatus::Firing, None) => {
                self.journal.append(JournalEvent::IncidentOpened {
                    incident_id: incident.incident_id.clone(),
                    alert_name: incident.alert_name.clone(),
                    event_time: incident.event_time.clone(),
                })?;
                self.lifecycle.insert(
                    key,
                    LifecycleState::Firing {
                        episode: incident.episode,
                        completed: false,
                        last_event_time: incident.event_time.clone(),
                    },
                );
                Ok((incident, DispatchOutcome::Accepted))
            }
            (AlertStatus::Resolved, None) => {
                self.journal.append(JournalEvent::IncidentRecovered {
                    incident_id: incident.incident_id.clone(),
                    event_time: incident.event_time.clone(),
                })?;
                self.lifecycle.insert(
                    key,
                    LifecycleState::Resolved {
                        episode: incident.episode,
                        last_event_time: incident.event_time.clone(),
                    },
                );
                Ok((incident, DispatchOutcome::Accepted))
            }
        }
    }

    /// Returns mutable journal access for the single worker's investigation.
    pub fn journal_mut(&mut self) -> &mut JournalStore {
        &mut self.journal
    }

    /// Returns the durable journal for replay and later investigation wiring.
    pub fn journal(&self) -> &JournalStore {
        &self.journal
    }

    /// Durably marks a firing episode complete after report admission.
    pub fn mark_completed(&mut self, incident_id: &str) -> Result<(), DispatchError> {
        self.mark_completed_with_outbox(incident_id, None)
    }

    /// Commits completion and an optional notification intent atomically.
    pub fn mark_completed_with_outbox(
        &mut self,
        incident_id: &str,
        outbox: Option<OutboxMessage>,
    ) -> Result<(), DispatchError> {
        self.journal.append_with_outbox(
            &[JournalEvent::IncidentCompleted {
                incident_id: incident_id.to_owned(),
            }],
            outbox.as_ref(),
        )?;
        let key = correlation_key(incident_id);
        if let Some(LifecycleState::Firing {
            episode,
            last_event_time,
            ..
        }) = self.lifecycle.get(&key)
        {
            self.lifecycle.insert(
                key,
                LifecycleState::Firing {
                    episode: *episode,
                    completed: true,
                    last_event_time: last_event_time.clone(),
                },
            );
        }
        Ok(())
    }
}

fn correlation_key(incident_id: &str) -> String {
    incident_id.rsplit_once("-episode-").map_or_else(
        || {
            incident_id
                .strip_prefix("incident-")
                .unwrap_or(incident_id)
                .to_owned()
        },
        |(key, _)| key.strip_prefix("incident-").unwrap_or(key).to_owned(),
    )
}

fn episode_number(incident_id: &str) -> u64 {
    incident_id
        .rsplit_once("-episode-")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(1)
}
