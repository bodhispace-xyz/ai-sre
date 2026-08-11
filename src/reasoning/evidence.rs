//! Immutable run-scoped evidence records shared by tools and reasoners.
//!
//! Records contain bounded, redacted tool output and stable IDs. They grant
//! no execution authority and are the only objects model reports may cite.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum redacted payload retained in one immutable evidence envelope.
pub const MAX_EVIDENCE_PAYLOAD_BYTES: usize = 64 * 1024;
/// Maximum redacted query expression retained with one evidence envelope.
pub const MAX_EVIDENCE_QUERY_BYTES: usize = 4 * 1024;
/// Maximum evidence envelopes retained by one run-scoped board.
pub const MAX_EVIDENCE_RECORDS: usize = 64;

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
    /// A response exceeded the immutable evidence envelope bound.
    #[error("evidence payload exceeds its configured bound")]
    PayloadTooLarge,
    /// A query exceeded the immutable provenance bound.
    #[error("evidence query exceeds its configured bound")]
    QueryTooLarge,
    /// The run-scoped board reached its record bound.
    #[error("evidence record budget exhausted")]
    RecordLimitExceeded,
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
        if self.records.len() >= MAX_EVIDENCE_RECORDS {
            return Err(EvidenceError::RecordLimitExceeded);
        }
        let query = redact_text(&query.into());
        if query.len() > MAX_EVIDENCE_QUERY_BYTES {
            return Err(EvidenceError::QueryTooLarge);
        }
        let payload = redact_payload(payload);
        if payload.is_empty() {
            return Err(EvidenceError::EmptyPayload);
        }
        if payload.len() > MAX_EVIDENCE_PAYLOAD_BYTES {
            return Err(EvidenceError::PayloadTooLarge);
        }
        let evidence_id = format!("evidence-{:04}", self.records.len() + 1);
        self.records.push(EvidenceRecord {
            evidence_id: evidence_id.clone(),
            source,
            query,
            payload,
        });
        Ok(evidence_id)
    }

    /// Returns immutable records for provider context export.
    pub fn records(&self) -> &[EvidenceRecord] {
        &self.records
    }
}

fn redact_payload(payload: Vec<u8>) -> Vec<u8> {
    let Ok(text) = String::from_utf8(payload) else {
        return b"[REDACTED_BINARY_PAYLOAD]".to_vec();
    };
    let mut value = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => value,
        Err(_) => return redact_text(&text).into_bytes(),
    };
    redact_json(&mut value);
    serde_json::to_vec(&value).unwrap_or_else(|_| redact_text(&text).into_bytes())
}

fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                let sensitive = key.to_ascii_lowercase().contains("token")
                    || key.to_ascii_lowercase().contains("secret")
                    || key.to_ascii_lowercase().contains("password")
                    || key.to_ascii_lowercase().contains("api_key")
                    || key.eq_ignore_ascii_case("authorization");
                if sensitive {
                    *child = serde_json::Value::String("[REDACTED]".to_owned());
                } else {
                    redact_json(child);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json),
        serde_json::Value::String(text) => *text = redact_text(text),
        _ => {}
    }
}

fn redact_text(input: &str) -> String {
    let mut redact_next = false;
    input
        .split_whitespace()
        .map(|word| {
            if redact_next {
                redact_next = false;
                return "[REDACTED]";
            }
            let lower = word.to_ascii_lowercase();
            if lower == "bearer" {
                redact_next = true;
                return word;
            }
            if ["token=", "password=", "secret=", "api_key=", "apikey="]
                .iter()
                .any(|prefix| lower.starts_with(prefix))
            {
                "[REDACTED]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
