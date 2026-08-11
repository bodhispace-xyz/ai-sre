//! Restart-safe append-only storage for incident journal entries.
//!
//! Each line is one complete JSON journal entry. The store validates every
//! existing line on open, assigns the next sequence, and syncs each append.

use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use thiserror::Error;

use super::journal::{IncidentJournal, JournalEntry, JournalEvent};

/// Fail-closed storage errors for the incident journal.
#[derive(Debug, Error)]
pub enum JournalStoreError {
    /// The journal file could not be opened or read.
    #[error("journal I/O failed")]
    Io(#[from] std::io::Error),
    /// A persisted entry was not valid JSON.
    #[error("journal entry is malformed")]
    InvalidEntry(#[from] serde_json::Error),
    /// A persisted sequence was not contiguous.
    #[error("journal sequence is not contiguous")]
    NonContiguousSequence,
}

/// JSON-lines journal store with deterministic replay.
#[derive(Debug)]
pub struct JournalStore {
    path: PathBuf,
    file: File,
    journal: IncidentJournal,
}

impl JournalStore {
    /// Opens an existing journal or creates an empty one.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JournalStoreError> {
        let path = path.as_ref().to_owned();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let mut journal = IncidentJournal::default();
        for line in BufReader::new(&file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: JournalEntry = serde_json::from_str(&line)?;
            if entry.sequence != journal.entries().len() as u64 {
                return Err(JournalStoreError::NonContiguousSequence);
            }
            journal.restore(entry);
        }
        Ok(Self {
            path,
            file,
            journal,
        })
    }

    /// Appends one event durably and returns its assigned sequence.
    pub fn append(&mut self, event: JournalEvent) -> Result<u64, JournalStoreError> {
        let sequence = self.journal.entries().len() as u64;
        let entry = JournalEntry { sequence, event };
        let line = serde_json::to_string(&entry)?;
        writeln!(&mut self.file, "{line}")?;
        self.file.sync_data()?;
        self.journal.restore(entry);
        Ok(sequence)
    }

    /// Returns the replayed in-memory journal.
    pub fn journal(&self) -> &IncidentJournal {
        &self.journal
    }

    /// Returns the backing path for diagnostics without exposing contents.
    pub fn path(&self) -> &Path {
        &self.path
    }
}
