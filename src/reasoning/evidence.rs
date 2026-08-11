//! Immutable run-scoped evidence records shared by tools and reasoners.
//!
//! Records contain bounded, redacted tool output and stable IDs. They grant
//! no execution authority and are the only objects model reports may cite.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Read-only source that produced an evidence record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceSource {
    /// Grafana Loki log query through the bounded `gcx` adapter.
    GrafanaLogs,
    /// Grafana Prometheus query through the bounded `gcx` adapter.
    GrafanaMetrics,
}

/// One immutable tool result committed to the evidence board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    /// Stable run-scoped identifier cited by provider reports.
    pub evidence_id: String,
    /// Read-only source capability.
    pub source: EvidenceSource,
    /// Query expression that produced this record.
    pub query: String,
    /// Bounded tool output.
    pub payload: Vec<u8>,
}

/// Evidence-board failures that preserve the previous immutable board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EvidenceError {
    /// A query produced no stable content to commit.
    #[error("evidence payload cannot be empty")]
    EmptyPayload,
}

/// Append-only run-scoped evidence board.
#[derive(Debug, Clone, Default)]
pub struct EvidenceBoard {
    records: Vec<EvidenceRecord>,
}

impl EvidenceBoard {
    /// Commits a bounded tool result under the next deterministic ID.
    pub fn commit(
        &mut self,
        source: EvidenceSource,
        query: impl Into<String>,
        payload: Vec<u8>,
    ) -> Result<String, EvidenceError> {
        if payload.is_empty() {
            return Err(EvidenceError::EmptyPayload);
        }
        let evidence_id = format!("evidence-{:04}", self.records.len() + 1);
        self.records.push(EvidenceRecord {
            evidence_id: evidence_id.clone(),
            source,
            query: query.into(),
            payload,
        });
        Ok(evidence_id)
    }

    /// Returns immutable records for provider context export.
    pub fn records(&self) -> &[EvidenceRecord] {
        &self.records
    }
}
