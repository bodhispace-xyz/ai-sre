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

use super::journal::{IncidentJournal, JournalEntry, JournalEvent};

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
}

/// A pending notification intent stored transactionally with incident state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxMessage {
    /// Stable idempotency key for the delivery.
    pub delivery_id: String,
    /// Redacted operator-facing message body.
    pub body: String,
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
        let connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS journal_events (
                sequence INTEGER PRIMARY KEY,
                event_json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS notification_outbox (
                delivery_id TEXT PRIMARY KEY,
                body TEXT NOT NULL,
                delivered_at_ms INTEGER
             );
             CREATE TABLE IF NOT EXISTS journal_markers (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
             );",
        )?;

        let mut journal = IncidentJournal::default();
        let mut statement = connection
            .prepare("SELECT sequence, event_json FROM journal_events ORDER BY sequence ASC")?;
        let rows = statement.query_map([], |row| {
            let sequence = u64::try_from(row.get::<_, i64>(0)?)
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, i64::MIN))?;
            let encoded: String = row.get(1)?;
            Ok((sequence, encoded))
        })?;
        for row in rows {
            let (sequence, encoded) = row?;
            if sequence != journal.entries().len() as u64 {
                return Err(JournalStoreError::NonContiguousSequence);
            }
            let event = serde_json::from_str::<JournalEvent>(&encoded)?;
            journal.restore(JournalEntry { sequence, event });
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
        let sequence = self.journal.entries().len() as u64;
        let encoded = serde_json::to_string(&event)?;
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO journal_events(sequence, event_json) VALUES (?1, ?2)",
            params![
                i64::try_from(sequence).map_err(|_| rusqlite::Error::ToSqlConversionFailure(
                    Box::new(std::io::Error::other("sequence overflow"))
                ))?,
                encoded
            ],
        )?;
        transaction.commit()?;
        self.journal.restore(JournalEntry { sequence, event });
        Ok(sequence)
    }

    /// Appends facts and an optional notification intent atomically.
    pub fn append_with_outbox(
        &mut self,
        events: &[JournalEvent],
        outbox: Option<&OutboxMessage>,
    ) -> Result<(), JournalStoreError> {
        let start = self.journal.entries().len() as u64;
        let transaction = self.connection.transaction()?;
        for (offset, event) in events.iter().enumerate() {
            let encoded = serde_json::to_string(event)?;
            transaction.execute(
                "INSERT INTO journal_events(sequence, event_json) VALUES (?1, ?2)",
                params![
                    i64::try_from(start + offset as u64).map_err(|_| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                            "sequence overflow",
                        )))
                    })?,
                    encoded
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
                event,
            });
        }
        Ok(())
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
