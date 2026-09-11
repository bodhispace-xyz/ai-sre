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
    /// Read-only desired-state snapshot from the configured Git repository.
    GitDesiredState,
    /// Read-only deployment history from the configured Git repository.
    DeploymentHistory,
    /// Read-only health snapshot from a server-owned health adapter.
    Health,
    /// Server-owned list of available read-only observability capabilities.
    ObservabilityMetadata,
}

/// Outcome metadata retained alongside every immutable evidence envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceMetadata {
    /// Whether the envelope contains complete data or a bounded failure marker.
    pub status: EvidenceStatus,
    /// Optional source freshness measured by the adapter.
    pub freshness_ms: Option<u64>,
    /// Whether the source was cut off by an output or policy bound.
    pub truncated: bool,
    /// Safe, non-vendor error classification when the source did not complete.
    pub error: Option<String>,
}

/// Stable classification for successful and failure evidence envelopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    /// The source returned complete bounded data.
    Complete,
    /// The source returned data that was cut off by a bound.
    Partial,
    /// The source returned no usable data.
    Empty,
    /// The request was rejected before or at the read-only boundary.
    Rejected,
    /// The source exceeded its wall-clock deadline.
    TimedOut,
    /// The source could not be reached or exited unexpectedly.
    Unavailable,
    /// The incident allowance prevented a request from running.
    BudgetExhausted,
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
    /// Freshness, truncation, and safe failure metadata.
    pub metadata: EvidenceMetadata,
}

impl EvidenceRecord {
    /// Binds the complete redacted record, including source, query, payload, and outcome metadata.
    pub fn content_digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(self).expect("evidence record serialization is infallible");
        let hex: String = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("sha256:{hex}")
    }
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
        self.commit_with_metadata(
            source,
            query,
            payload,
            EvidenceMetadata {
                status: EvidenceStatus::Complete,
                freshness_ms: None,
                truncated: false,
                error: None,
            },
        )
    }

    /// Commits a bounded successful or failure envelope without raw provider details.
    pub fn commit_with_metadata(
        &mut self,
        source: EvidenceSource,
        query: impl Into<String>,
        payload: Vec<u8>,
        metadata: EvidenceMetadata,
    ) -> Result<String, EvidenceError> {
        if payload.is_empty() {
            return Err(EvidenceError::EmptyPayload);
        }
        if self.records.len() >= MAX_EVIDENCE_RECORDS {
            return Err(EvidenceError::RecordLimitExceeded);
        }
        if payload.len() > MAX_EVIDENCE_PAYLOAD_BYTES {
            return Err(EvidenceError::PayloadTooLarge);
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
            metadata,
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
                let sensitive = is_sensitive_key(key);
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
    let chars = input.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    while index < chars.len() {
        if let Some((key_len, is_authorization)) = sensitive_assignment(&chars, index) {
            output.extend(chars[index..index + key_len].iter());
            index += key_len;
            while index < chars.len() && chars[index].is_whitespace() {
                output.push(chars[index]);
                index += 1;
            }
            if index < chars.len() && matches!(chars[index], ':' | '=') {
                output.push(chars[index]);
                index += 1;
            }
            while index < chars.len() && chars[index].is_whitespace() {
                output.push(chars[index]);
                index += 1;
            }
            if index < chars.len() && matches!(chars[index], '\"' | '\'') {
                let quote = chars[index];
                output.push(quote);
                index += 1;
                output.push_str("[REDACTED]");
                while index < chars.len() && chars[index] != quote {
                    index += 1;
                }
                if index < chars.len() {
                    output.push(quote);
                    index += 1;
                }
            } else {
                output.push_str("[REDACTED]");
                while index < chars.len()
                    && if is_authorization {
                        !matches!(chars[index], '\n' | '\r')
                    } else {
                        !chars[index].is_whitespace() && !matches!(chars[index], ',' | '}' | ']')
                    }
                {
                    index += 1;
                }
            }
            continue;
        }
        if let Some(token_len) = bearer_prefix(&chars, index) {
            output.extend(chars[index..index + token_len].iter());
            index += token_len;
            while index < chars.len() && chars[index].is_whitespace() {
                output.push(chars[index]);
                index += 1;
            }
            if index < chars.len() {
                output.push_str("[REDACTED]");
                while index < chars.len() && !chars[index].is_whitespace() {
                    index += 1;
                }
            }
            continue;
        }
        output.push(chars[index]);
        index += 1;
    }
    output
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '.'], "_");
    normalized.contains("token")
        || normalized.contains("secret")
        || normalized.contains("password")
        || normalized.contains("apikey")
        || normalized.contains("api_key")
        || normalized.contains("authorization")
        || normalized.contains("credential")
        || normalized.contains("private_key")
}

fn sensitive_assignment(chars: &[char], index: usize) -> Option<(usize, bool)> {
    const KEYS: [&str; 9] = [
        "access_token",
        "authorization",
        "private_key",
        "credential",
        "password",
        "api_key",
        "apikey",
        "secret",
        "token",
    ];
    let boundary = index == 0 || !chars[index - 1].is_ascii_alphanumeric();
    if !boundary {
        return None;
    }
    KEYS.iter().find_map(|key| {
        let key_chars = key.chars().collect::<Vec<_>>();
        if chars.get(index..index + key_chars.len())? != key_chars.as_slice()
            && !chars
                .get(index..index + key_chars.len())?
                .iter()
                .zip(key_chars.iter())
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        {
            return None;
        }
        let mut cursor = index + key_chars.len();
        while cursor < chars.len() && chars[cursor].is_whitespace() {
            cursor += 1;
        }
        if !matches!(chars.get(cursor), Some(':' | '=')) {
            return None;
        }
        Some((key_chars.len(), *key == "authorization"))
    })
}

fn bearer_prefix(chars: &[char], index: usize) -> Option<usize> {
    const BEARER: &str = "bearer";
    let boundary = index == 0 || !chars[index - 1].is_ascii_alphanumeric();
    let candidate = chars.get(index..index + BEARER.len())?;
    if boundary
        && candidate
            .iter()
            .zip(BEARER.chars())
            .all(|(left, right)| left.eq_ignore_ascii_case(&right))
        && chars
            .get(index + BEARER.len())
            .is_some_and(|character| character.is_whitespace())
    {
        Some(BEARER.len())
    } else {
        None
    }
}
