//! SQLite-backed durable storage for incident facts and notification intents.
//!
//! The store configures WAL and full synchronous commits, replays the
//! append-only event table into the pure journal projection, and keeps
//! notification intents in the same database for at-least-once delivery.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use super::journal::{IncidentJournal, JournalContext, JournalEntry, JournalEvent};

/// Fail-closed storage errors for the incident journal and outbox.
#[derive(Debug, Error)]
pub enum JournalStoreError {
    /// SQLite could not open, migrate, read, or commit state.
    #[error("journal database operation failed")]
    Sqlite(#[from] rusqlite::Error),
    /// A persisted event could not be serialized or decoded.
    #[error("journal event encoding failed")]
    Encoding(#[from] serde_json::Error),
    /// A persisted sequence was not contiguous.
    #[error("journal sequence is not contiguous")]
    NonContiguousSequence,
    /// A durable global or incident cost ceiling would be exceeded.
    #[error("durable cost budget exhausted")]
    BudgetExhausted,
    /// A reconciliation referenced no durable reservation.
    #[error("durable cost reservation was not found")]
    ReservationNotFound,
}

/// A pending notification intent stored transactionally with incident state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxMessage {
    /// Stable idempotency key for the delivery.
    pub delivery_id: String,
    /// Redacted operator-facing message body.
    pub body: String,
}

/// Durable redacted HTML report used to rebuild the operator page after restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredReport {
    /// Stable incident identity.
    pub incident_id: String,
    /// Already-rendered, escaped and redacted report page.
    pub html: String,
}

/// SQLite journal store with deterministic replay and a transactional outbox.
pub struct JournalStore {
    path: PathBuf,
    connection: Connection,
    journal: IncidentJournal,
}

impl fmt::Debug for JournalStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JournalStore")
            .field("path", &self.path)
            .field("entries", &self.journal.entries().len())
            .finish()
    }
}

impl JournalStore {
    /// Opens or creates the SQLite database and validates its replay stream.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JournalStoreError> {
        let path = path.as_ref().to_owned();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                JournalStoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
            })?;
        }
        let connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS journal_events (
                sequence INTEGER PRIMARY KEY,
                event_json TEXT NOT NULL,
                incident_id TEXT,
                run_id TEXT
             );
             CREATE TABLE IF NOT EXISTS notification_outbox (
                delivery_id TEXT PRIMARY KEY,
                body TEXT NOT NULL,
                delivered_at_ms INTEGER
             );
             CREATE TABLE IF NOT EXISTS incident_reports (
                incident_id TEXT PRIMARY KEY,
                html TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS journal_markers (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS budget_ledgers (
                scope TEXT PRIMARY KEY,
                ceiling_micro_usd INTEGER NOT NULL,
                reserved_micro_usd INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS budget_reservations (
                reservation_id TEXT PRIMARY KEY,
                amount_micro_usd INTEGER NOT NULL,
                actual_micro_usd INTEGER,
                created_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS budget_reservation_scopes (
                reservation_id TEXT NOT NULL,
                scope TEXT NOT NULL,
                PRIMARY KEY (reservation_id, scope),
                FOREIGN KEY (reservation_id) REFERENCES budget_reservations(reservation_id)
             );",
        )?;
        // Upgrade databases created by the U0 schema before scoped facts.
        let _ = connection.execute("ALTER TABLE journal_events ADD COLUMN incident_id TEXT", []);
        let _ = connection.execute("ALTER TABLE journal_events ADD COLUMN run_id TEXT", []);

        let mut journal = IncidentJournal::default();
        let mut statement = connection.prepare(
            "SELECT sequence, event_json, incident_id, run_id
             FROM journal_events ORDER BY sequence ASC",
        )?;
        let rows = statement.query_map([], |row| {
            let sequence = u64::try_from(row.get::<_, i64>(0)?)
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, i64::MIN))?;
            let encoded: String = row.get(1)?;
            let incident_id: Option<String> = row.get(2)?;
            let run_id: Option<String> = row.get(3)?;
            Ok((sequence, encoded, incident_id, run_id))
        })?;
        for row in rows {
            let (sequence, encoded, incident_id, run_id) = row?;
            if sequence != journal.entries().len() as u64 {
                return Err(JournalStoreError::NonContiguousSequence);
            }
            let event = serde_json::from_str::<JournalEvent>(&encoded)?;
            journal.restore(JournalEntry {
                sequence,
                context: incident_id
                    .zip(run_id)
                    .map(|(incident_id, run_id)| JournalContext {
                        incident_id,
                        run_id,
                    }),
                event,
            });
        }

        drop(statement);
        Ok(Self {
            path,
            connection,
            journal,
        })
    }

    /// Appends one event in a full-synchronous SQLite transaction.
    pub fn append(&mut self, event: JournalEvent) -> Result<u64, JournalStoreError> {
        self.append_scoped(event, None)
    }

    /// Appends one event with an optional incident/run scope.
    pub fn append_scoped(
        &mut self,
        event: JournalEvent,
        context: Option<&JournalContext>,
    ) -> Result<u64, JournalStoreError> {
        let sequence = self.journal.entries().len() as u64;
        let encoded = serde_json::to_string(&event)?;
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO journal_events(sequence, event_json, incident_id, run_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                i64::try_from(sequence).map_err(|_| rusqlite::Error::ToSqlConversionFailure(
                    Box::new(std::io::Error::other("sequence overflow"))
                ))?,
                encoded,
                context.map(|context| context.incident_id.as_str()),
                context.map(|context| context.run_id.as_str()),
            ],
        )?;
        transaction.commit()?;
        self.journal.restore(JournalEntry {
            sequence,
            context: context.cloned(),
            event,
        });
        Ok(sequence)
    }

    /// Appends a checkpoint once, making retries after an ambiguous commit idempotent.
    pub fn append_checkpoint_scoped(
        &mut self,
        checkpoint_id: &str,
        events: &[JournalEvent],
        context: Option<&JournalContext>,
    ) -> Result<bool, JournalStoreError> {
        let marker_key = format!("journal-checkpoint:{checkpoint_id}");
        let transaction = self.connection.transaction()?;
        let exists = transaction
            .query_row(
                "SELECT 1 FROM journal_markers WHERE key = ?1",
                params![marker_key],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if exists {
            return Ok(false);
        }
        let start = self.journal.entries().len() as u64;
        for (offset, event) in events.iter().enumerate() {
            transaction.execute(
                "INSERT INTO journal_events(sequence, event_json, incident_id, run_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    i64::try_from(start + offset as u64).map_err(|_| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                            "sequence overflow",
                        )))
                    })?,
                    serde_json::to_string(event)?,
                    context.map(|context| context.incident_id.as_str()),
                    context.map(|context| context.run_id.as_str()),
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO journal_markers(key, value) VALUES (?1, ?2)",
            params![marker_key, checkpoint_id],
        )?;
        transaction.commit()?;
        for (offset, event) in events.iter().cloned().enumerate() {
            self.journal.restore(JournalEntry {
                sequence: start + offset as u64,
                context: context.cloned(),
                event,
            });
        }
        Ok(true)
    }

    /// Returns whether a scoped tool request is still unresolved.
    pub fn tool_request_pending(&self, context: &JournalContext, call_id: &str) -> bool {
        let requested = self.journal.entries().iter().any(|entry| {
            entry.context.as_ref() == Some(context)
                && matches!(&entry.event, JournalEvent::ToolRequested { call_id: id, .. } if id == call_id)
        });
        let completed = self.journal.entries().iter().any(|entry| {
            entry.context.as_ref() == Some(context)
                && matches!(&entry.event, JournalEvent::ToolContext { call_id: id, .. } if id == call_id)
        });
        requested && !completed
    }

    /// Returns whether a scoped tool call has already entered the durable log.
    pub fn tool_request_recorded(&self, context: &JournalContext, call_id: &str) -> bool {
        self.journal.entries().iter().any(|entry| {
            entry.context.as_ref() == Some(context)
                && matches!(&entry.event, JournalEvent::ToolRequested { call_id: id, .. } if id == call_id)
        })
    }

    /// Appends facts and an optional notification intent atomically.
    pub fn append_with_outbox(
        &mut self,
        events: &[JournalEvent],
        outbox: Option<&OutboxMessage>,
    ) -> Result<(), JournalStoreError> {
        self.append_with_outbox_scoped(events, outbox, None)
    }

    /// Appends facts and an optional notification intent with one scope.
    pub fn append_with_outbox_scoped(
        &mut self,
        events: &[JournalEvent],
        outbox: Option<&OutboxMessage>,
        context: Option<&JournalContext>,
    ) -> Result<(), JournalStoreError> {
        let start = self.journal.entries().len() as u64;
        let transaction = self.connection.transaction()?;
        for (offset, event) in events.iter().enumerate() {
            let encoded = serde_json::to_string(event)?;
            transaction.execute(
                "INSERT INTO journal_events(sequence, event_json, incident_id, run_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    i64::try_from(start + offset as u64).map_err(|_| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                            "sequence overflow",
                        )))
                    })?,
                    encoded,
                    context.map(|context| context.incident_id.as_str()),
                    context.map(|context| context.run_id.as_str()),
                ],
            )?;
        }
        if let Some(message) = outbox {
            transaction.execute(
                "INSERT INTO notification_outbox(delivery_id, body, delivered_at_ms)
                 VALUES (?1, ?2, NULL)
                 ON CONFLICT(delivery_id) DO NOTHING",
                params![message.delivery_id, message.body],
            )?;
        }
        transaction.commit()?;
        for (offset, event) in events.iter().cloned().enumerate() {
            self.journal.restore(JournalEntry {
                sequence: start + offset as u64,
                context: context.cloned(),
                event,
            });
        }
        Ok(())
    }

    /// Commits lifecycle facts, notification intent, and a redacted page atomically.
    pub fn append_with_outbox_and_report(
        &mut self,
        events: &[JournalEvent],
        outbox: Option<&OutboxMessage>,
        report: &StoredReport,
    ) -> Result<(), JournalStoreError> {
        let start = self.journal.entries().len() as u64;
        let transaction = self.connection.transaction()?;
        for (offset, event) in events.iter().enumerate() {
            transaction.execute(
                "INSERT INTO journal_events(sequence, event_json, incident_id, run_id) VALUES (?1, ?2, NULL, NULL)",
                params![
                    i64::try_from(start + offset as u64).map_err(|_| rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other("sequence overflow"))))?,
                    serde_json::to_string(event)?
                ],
            )?;
        }
        if let Some(message) = outbox {
            transaction.execute(
                "INSERT INTO notification_outbox(delivery_id, body, delivered_at_ms) VALUES (?1, ?2, NULL) ON CONFLICT(delivery_id) DO NOTHING",
                params![message.delivery_id, message.body],
            )?;
        }
        transaction.execute(
            "INSERT INTO incident_reports(incident_id, html) VALUES (?1, ?2) ON CONFLICT(incident_id) DO UPDATE SET html = excluded.html",
            params![report.incident_id, report.html],
        )?;
        transaction.commit()?;
        for (offset, event) in events.iter().cloned().enumerate() {
            self.journal.restore(JournalEntry {
                sequence: start + offset as u64,
                context: None,
                event,
            });
        }
        Ok(())
    }

    /// Loads the newest durable reports for page hydration.
    pub fn stored_reports(&self, limit: usize) -> Result<Vec<StoredReport>, JournalStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT incident_id, html FROM incident_reports ORDER BY rowid DESC LIMIT ?1",
        )?;
        let rows =
            statement.query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok(StoredReport {
                    incident_id: row.get(0)?,
                    html: row.get(1)?,
                })
            })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Reserves one worst-case cost atomically across all configured scopes.
    /// Repeating the same reservation ID is idempotent after a retry.
    pub fn reserve_cost(
        &mut self,
        reservation_id: &str,
        amount_micro_usd: u64,
        scopes: &[(&str, u64)],
        created_at_ms: u64,
    ) -> Result<(), JournalStoreError> {
        let transaction = self.connection.transaction()?;
        let existing = transaction
            .query_row(
                "SELECT 1 FROM budget_reservations WHERE reservation_id = ?1",
                params![reservation_id],
                |_| Ok(()),
            )
            .optional()?;
        if existing.is_some() {
            return Ok(());
        }
        let amount = i64::try_from(amount_micro_usd).map_err(|_| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                "budget amount overflow",
            )))
        })?;
        for (scope, ceiling_micro_usd) in scopes {
            let ceiling = i64::try_from(*ceiling_micro_usd).map_err(|_| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                    "budget ceiling overflow",
                )))
            })?;
            let reserved: Option<i64> = transaction
                .query_row(
                    "SELECT reserved_micro_usd FROM budget_ledgers WHERE scope = ?1",
                    params![scope],
                    |row| row.get(0),
                )
                .optional()?;
            if reserved.unwrap_or_default().saturating_add(amount) > ceiling {
                return Err(JournalStoreError::BudgetExhausted);
            }
        }
        transaction.execute(
            "INSERT INTO budget_reservations
             (reservation_id, amount_micro_usd, actual_micro_usd, created_at_ms)
             VALUES (?1, ?2, NULL, ?3)",
            params![
                reservation_id,
                amount,
                i64::try_from(created_at_ms).unwrap_or(i64::MAX)
            ],
        )?;
        for (scope, ceiling_micro_usd) in scopes {
            let ceiling = i64::try_from(*ceiling_micro_usd).map_err(|_| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                    "budget ceiling overflow",
                )))
            })?;
            transaction.execute(
                "INSERT INTO budget_ledgers(scope, ceiling_micro_usd, reserved_micro_usd)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(scope) DO UPDATE SET reserved_micro_usd =
                   budget_ledgers.reserved_micro_usd + excluded.reserved_micro_usd",
                params![scope, ceiling, amount],
            )?;
            transaction.execute(
                "INSERT INTO budget_reservation_scopes(reservation_id, scope)
                 VALUES (?1, ?2)",
                params![reservation_id, scope],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Reconciles a reservation when trustworthy provider usage is available.
    /// Unknown usage is intentionally left reserved by passing `None`.
    pub fn reconcile_cost(
        &mut self,
        reservation_id: &str,
        actual_micro_usd: Option<u64>,
    ) -> Result<bool, JournalStoreError> {
        let Some(actual_micro_usd) = actual_micro_usd else {
            return Ok(false);
        };
        let transaction = self.connection.transaction()?;
        let Some((reserved, already_actual)) = transaction
            .query_row(
                "SELECT amount_micro_usd, actual_micro_usd
                 FROM budget_reservations WHERE reservation_id = ?1",
                params![reservation_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .optional()?
        else {
            return Err(JournalStoreError::ReservationNotFound);
        };
        if already_actual.is_some() {
            return Ok(false);
        }
        let actual = i64::try_from(actual_micro_usd).map_err(|_| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                "actual budget overflow",
            )))
        })?;
        let mut scopes = transaction
            .prepare("SELECT scope FROM budget_reservation_scopes WHERE reservation_id = ?1")?;
        let scope_rows =
            scopes.query_map(params![reservation_id], |row| row.get::<_, String>(0))?;
        let scope_names = scope_rows.collect::<Result<Vec<_>, _>>()?;
        drop(scopes);
        for scope in scope_names {
            transaction.execute(
                "UPDATE budget_ledgers
                 SET reserved_micro_usd = reserved_micro_usd - ?2 + ?3
                 WHERE scope = ?1",
                params![scope, reserved, actual],
            )?;
        }
        transaction.execute(
            "UPDATE budget_reservations SET actual_micro_usd = ?2
             WHERE reservation_id = ?1 AND actual_micro_usd IS NULL",
            params![reservation_id, actual],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    /// Returns undelivered notification intents in stable insertion order.
    pub fn pending_outbox(&self) -> Result<Vec<OutboxMessage>, JournalStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT delivery_id, body FROM notification_outbox
             WHERE delivered_at_ms IS NULL ORDER BY rowid ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(OutboxMessage {
                delivery_id: row.get(0)?,
                body: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Marks one notification intent delivered after the remote accepts it.
    pub fn mark_outbox_delivered(
        &mut self,
        delivery_id: &str,
        at_ms: u64,
    ) -> Result<bool, JournalStoreError> {
        let updated = self.connection.execute(
            "UPDATE notification_outbox SET delivered_at_ms = ?2
             WHERE delivery_id = ?1 AND delivered_at_ms IS NULL",
            params![
                delivery_id,
                i64::try_from(at_ms).map_err(|_| rusqlite::Error::ToSqlConversionFailure(
                    Box::new(std::io::Error::other("timestamp overflow"))
                ))?
            ],
        )?;
        Ok(updated == 1)
    }

    /// Returns the replayed in-memory journal.
    pub fn journal(&self) -> &IncidentJournal {
        &self.journal
    }

    /// Rebuilds one scoped efficiency projection after replay.
    pub fn efficiency_projection(
        &self,
        context: &JournalContext,
    ) -> super::journal::EfficiencyProjection {
        self.journal.project_scoped(context)
    }

    /// Returns the backing database path without exposing contents.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns whether the database already contains any durable facts.
    pub fn is_empty(&self) -> bool {
        self.journal.entries().is_empty()
    }

    /// Reads a durable scalar marker used for deployment identity checks.
    pub fn marker(&self, key: &str) -> Result<Option<String>, JournalStoreError> {
        self.connection
            .query_row(
                "SELECT value FROM journal_markers WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }
}
