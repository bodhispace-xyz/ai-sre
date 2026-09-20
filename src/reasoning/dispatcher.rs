//! Single-writer incident dispatch with replayed lifecycle correlation.
//!
//! The dispatcher accepts duplicate signals only when they repeat the current
//! lifecycle state. Resolved episodes close a correlation key, and a later
//! firing creates a new episode rather than suppressing a real recurrence.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::transport::IntakeBatch;

use super::{
    incident::{AlertStatus, IncidentSignal, parse_rfc3339_millis},
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
    /// The event was durable but does not create another investigation.
    Resumed,
}

/// Errors while recording an accepted webhook batch.
#[derive(Debug, Error)]
pub enum DispatchError {
    /// The durable lifecycle fact could not be written.
    #[error("incident dispatch journal write failed")]
    Journal(#[from] JournalStoreError),
    /// The signal lacks a valid lifecycle timestamp.
    #[error("incident signal has an invalid event timestamp")]
    InvalidSignal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LifecycleState {
    Firing {
        episode: u64,
        completed: bool,
        last_event_time: String,
        last_event_time_ms: Option<i128>,
        last_source_event_id: String,
    },
    Resolved {
        episode: u64,
        last_event_time: String,
        last_event_time_ms: Option<i128>,
        last_source_event_id: String,
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
                    source_event_id,
                    ..
                } => {
                    lifecycle.insert(
                        correlation_key(incident_id),
                        LifecycleState::Firing {
                            episode: episode_number(incident_id),
                            completed: false,
                            last_event_time: event_time.clone(),
                            last_event_time_ms: parse_rfc3339_millis(event_time),
                            last_source_event_id: source_event_id.clone(),
                        },
                    );
                }
                JournalEvent::IncidentRecovered {
                    incident_id,
                    event_time,
                    source_event_id,
                } => {
                    lifecycle.insert(
                        correlation_key(incident_id),
                        LifecycleState::Resolved {
                            episode: episode_number(incident_id),
                            last_event_time: event_time.clone(),
                            last_event_time_ms: parse_rfc3339_millis(event_time),
                            last_source_event_id: source_event_id.clone(),
                        },
                    );
                }
                JournalEvent::IncidentCompleted { incident_id } => {
                    let key = correlation_key(incident_id);
                    if let Some(LifecycleState::Firing {
                        episode,
                        last_event_time,
                        last_event_time_ms,
                        last_source_event_id,
                        ..
                    }) = lifecycle.get(&key)
                    {
                        lifecycle.insert(
                            key,
                            LifecycleState::Firing {
                                episode: *episode,
                                completed: true,
                                last_event_time: last_event_time.clone(),
                                last_event_time_ms: *last_event_time_ms,
                                last_source_event_id: last_source_event_id.clone(),
                            },
                        );
                    }
                }
                JournalEvent::IncidentResumed {
                    incident_id,
                    event_time,
                    source_event_id,
                } => {
                    if let Some(LifecycleState::Firing {
                        episode, completed, ..
                    }) = lifecycle.get(&correlation_key(incident_id))
                    {
                        lifecycle.insert(
                            correlation_key(incident_id),
                            LifecycleState::Firing {
                                episode: *episode,
                                completed: *completed,
                                last_event_time: event_time.clone(),
                                last_event_time_ms: parse_rfc3339_millis(event_time),
                                last_source_event_id: source_event_id.clone(),
                            },
                        );
                    }
                }
                JournalEvent::AlertDeduplicated { .. }
                | JournalEvent::AlertOutOfOrder { .. }
                | JournalEvent::EvidenceCommitted { .. }
                | JournalEvent::ToolContext { .. }
                | JournalEvent::ToolRequested { .. }
                | JournalEvent::PhaseStarted { .. }
                | JournalEvent::PhaseFinished { .. }
                | JournalEvent::ProviderAttempt(_)
                | JournalEvent::Terminal { .. }
                | JournalEvent::DeploymentQualification { .. }
                | JournalEvent::ManualRepairPrepared { .. }
                | JournalEvent::ManualValidation { .. }
                | JournalEvent::ManualHandoffAcknowledged { .. }
                | JournalEvent::ManualRepairValidated { .. } => {}
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
        if incident.event_time_ms.is_none() {
            return Err(DispatchError::InvalidSignal);
        }
        let current = self.lifecycle.get(&key).cloned();
        if let Some(last_event_time_ms) = current.as_ref().and_then(|state| match state {
            LifecycleState::Firing {
                last_event_time_ms, ..
            }
            | LifecycleState::Resolved {
                last_event_time_ms, ..
            } => *last_event_time_ms,
        }) {
            if incident.event_time_ms < Some(last_event_time_ms) {
                self.journal.append(JournalEvent::AlertOutOfOrder {
                    incident_id: incident.incident_id.clone(),
                    status: incident.status,
                    event_time: incident.event_time.clone(),
                    source_event_id: incident.source_event_id.clone(),
                })?;
                return Ok((incident, DispatchOutcome::Deduplicated));
            }
        }
        match (incident.status, current) {
            (
                AlertStatus::Firing,
                Some(LifecycleState::Firing {
                    completed: true,
                    episode: _,
                    last_source_event_id,
                    ..
                }),
            ) if incident.source_event_id == last_source_event_id => {
                self.journal.append(JournalEvent::AlertDeduplicated {
                    incident_id: incident.incident_id.clone(),
                    event_time: incident.event_time.clone(),
                    source_event_id: incident.source_event_id.clone(),
                })?;
                Ok((incident, DispatchOutcome::Deduplicated))
            }
            (AlertStatus::Resolved, Some(LifecycleState::Resolved { episode, .. })) => {
                incident.episode = episode;
                incident.incident_id = if episode == 1 {
                    format!("incident-{key}")
                } else {
                    format!("incident-{key}-episode-{episode}")
                };
                self.journal.append(JournalEvent::AlertDeduplicated {
                    incident_id: incident.incident_id.clone(),
                    event_time: incident.event_time.clone(),
                    source_event_id: incident.source_event_id.clone(),
                })?;
                self.lifecycle.insert(
                    key,
                    LifecycleState::Resolved {
                        episode,
                        last_event_time: incident.event_time.clone(),
                        last_event_time_ms: incident.event_time_ms,
                        last_source_event_id: incident.source_event_id.clone(),
                    },
                );
                Ok((incident, DispatchOutcome::Deduplicated))
            }
            (
                AlertStatus::Firing,
                Some(LifecycleState::Firing {
                    completed: true,
                    episode,
                    ..
                }),
            ) => {
                incident.episode = episode + 1;
                incident.incident_id = format!("incident-{}-episode-{}", key, incident.episode);
                self.journal.append(JournalEvent::IncidentOpened {
                    incident_id: incident.incident_id.clone(),
                    alert_name: incident.alert_name.clone(),
                    labels: incident.labels.clone(),
                    annotations: incident.annotations.clone(),
                    event_time: incident.event_time.clone(),
                    source_event_id: incident.source_event_id.clone(),
                })?;
                self.lifecycle.insert(
                    key,
                    LifecycleState::Firing {
                        episode: incident.episode,
                        completed: false,
                        last_event_time: incident.event_time.clone(),
                        last_event_time_ms: incident.event_time_ms,
                        last_source_event_id: incident.source_event_id.clone(),
                    },
                );
                Ok((incident, DispatchOutcome::Accepted))
            }
            (
                AlertStatus::Firing,
                Some(LifecycleState::Firing {
                    episode,
                    completed: false,
                    ..
                }),
            ) => {
                incident.episode = episode;
                incident.incident_id = if episode == 1 {
                    format!("incident-{key}")
                } else {
                    format!("incident-{key}-episode-{episode}")
                };
                self.journal.append(JournalEvent::IncidentResumed {
                    incident_id: incident.incident_id.clone(),
                    event_time: incident.event_time.clone(),
                    source_event_id: incident.source_event_id.clone(),
                })?;
                self.lifecycle.insert(
                    key,
                    LifecycleState::Firing {
                        episode,
                        completed: false,
                        last_event_time: incident.event_time.clone(),
                        last_event_time_ms: incident.event_time_ms,
                        last_source_event_id: incident.source_event_id.clone(),
                    },
                );
                Ok((incident, DispatchOutcome::Resumed))
            }
            (AlertStatus::Firing, Some(LifecycleState::Resolved { episode, .. })) => {
                incident.episode = episode + 1;
                incident.incident_id = format!("incident-{}-episode-{}", key, incident.episode);
                self.journal.append(JournalEvent::IncidentOpened {
                    incident_id: incident.incident_id.clone(),
                    alert_name: incident.alert_name.clone(),
                    labels: incident.labels.clone(),
                    annotations: incident.annotations.clone(),
                    event_time: incident.event_time.clone(),
                    source_event_id: incident.source_event_id.clone(),
                })?;
                self.lifecycle.insert(
                    key,
                    LifecycleState::Firing {
                        episode: incident.episode,
                        completed: false,
                        last_event_time: incident.event_time.clone(),
                        last_event_time_ms: incident.event_time_ms,
                        last_source_event_id: incident.source_event_id.clone(),
                    },
                );
                Ok((incident, DispatchOutcome::Accepted))
            }
            (AlertStatus::Resolved, Some(LifecycleState::Firing { episode, .. })) => {
                incident.episode = episode;
                incident.incident_id = if episode == 1 {
                    format!("incident-{key}")
                } else {
                    format!("incident-{key}-episode-{episode}")
                };
                self.journal.append(JournalEvent::IncidentRecovered {
                    incident_id: incident.incident_id.clone(),
                    event_time: incident.event_time.clone(),
                    source_event_id: incident.source_event_id.clone(),
                })?;
                self.lifecycle.insert(
                    key,
                    LifecycleState::Resolved {
                        episode,
                        last_event_time: incident.event_time.clone(),
                        last_event_time_ms: incident.event_time_ms,
                        last_source_event_id: incident.source_event_id.clone(),
                    },
                );
                Ok((incident, DispatchOutcome::Accepted))
            }
            (AlertStatus::Firing, None) => {
                self.journal.append(JournalEvent::IncidentOpened {
                    incident_id: incident.incident_id.clone(),
                    alert_name: incident.alert_name.clone(),
                    labels: incident.labels.clone(),
                    annotations: incident.annotations.clone(),
                    event_time: incident.event_time.clone(),
                    source_event_id: incident.source_event_id.clone(),
                })?;
                self.lifecycle.insert(
                    key,
                    LifecycleState::Firing {
                        episode: incident.episode,
                        completed: false,
                        last_event_time: incident.event_time.clone(),
                        last_event_time_ms: incident.event_time_ms,
                        last_source_event_id: incident.source_event_id.clone(),
                    },
                );
                Ok((incident, DispatchOutcome::Accepted))
            }
            (AlertStatus::Resolved, None) => {
                self.journal.append(JournalEvent::IncidentRecovered {
                    incident_id: incident.incident_id.clone(),
                    event_time: incident.event_time.clone(),
                    source_event_id: incident.source_event_id.clone(),
                })?;
                self.lifecycle.insert(
                    key,
                    LifecycleState::Resolved {
                        episode: incident.episode,
                        last_event_time: incident.event_time.clone(),
                        last_event_time_ms: incident.event_time_ms,
                        last_source_event_id: incident.source_event_id.clone(),
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

    /// Reconstructs firing episodes that were admitted before a restart but
    /// never reached a durable terminal completion.
    pub fn pending_investigations(&self) -> Vec<IncidentSignal> {
        self.lifecycle
            .iter()
            .filter_map(|(key, state)| {
                let LifecycleState::Firing {
                    episode,
                    completed: false,
                    last_event_time,
                    last_event_time_ms,
                    last_source_event_id,
                    ..
                } = state
                else {
                    return None;
                };
                let opened = self
                    .journal
                    .journal()
                    .entries()
                    .iter()
                    .rev()
                    .find_map(|entry| match &entry.event {
                        JournalEvent::IncidentOpened {
                            incident_id,
                            alert_name,
                            labels,
                            annotations,
                            ..
                        } if correlation_key(incident_id) == *key => Some((
                            incident_id.clone(),
                            alert_name.clone(),
                            labels.clone(),
                            annotations.clone(),
                        )),
                        _ => None,
                    })?;
                let incident_id = if *episode == 1 {
                    opened.0
                } else {
                    format!("incident-{key}-episode-{episode}")
                };
                Some(IncidentSignal {
                    correlation_id: key.clone(),
                    incident_id,
                    episode: *episode,
                    alert_name: opened.1,
                    status: AlertStatus::Firing,
                    labels: opened.2,
                    annotations: opened.3,
                    event_time: last_event_time.clone(),
                    source_event_id: last_source_event_id.clone(),
                    event_time_ms: *last_event_time_ms,
                })
            })
            .collect()
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
            last_event_time_ms,
            last_source_event_id,
            ..
        }) = self.lifecycle.get(&key)
        {
            self.lifecycle.insert(
                key,
                LifecycleState::Firing {
                    episode: *episode,
                    completed: true,
                    last_event_time: last_event_time.clone(),
                    last_event_time_ms: *last_event_time_ms,
                    last_source_event_id: last_source_event_id.clone(),
                },
            );
        }
        Ok(())
    }

    /// Commits completion, notification intent, and the redacted page together.
    pub fn mark_completed_with_report(
        &mut self,
        incident_id: &str,
        outbox: Option<OutboxMessage>,
        report: super::storage::StoredReport,
    ) -> Result<(), DispatchError> {
        self.journal.append_with_outbox_and_report(
            &[JournalEvent::IncidentCompleted {
                incident_id: incident_id.to_owned(),
            }],
            outbox.as_ref(),
            &report,
        )?;
        let key = correlation_key(incident_id);
        if let Some(LifecycleState::Firing {
            episode,
            last_event_time,
            last_event_time_ms,
            last_source_event_id,
            ..
        }) = self.lifecycle.get(&key)
        {
            self.lifecycle.insert(
                key,
                LifecycleState::Firing {
                    episode: *episode,
                    completed: true,
                    last_event_time: last_event_time.clone(),
                    last_event_time_ms: *last_event_time_ms,
                    last_source_event_id: last_source_event_id.clone(),
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
