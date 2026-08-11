//! Provider-neutral advisory artifacts and their fail-closed validation.
//!
//! These contracts accept model output but grant no runtime authority. Evidence
//! references must resolve against the immutable evidence board for the run.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A citation to evidence already present on the immutable run-scoped board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRef {
    /// The stable identifier assigned when the evidence was committed.
    pub evidence_id: String,
}

/// An advisory diagnosis whose citations must resolve before it is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticReport {
    /// The provider's concise diagnosis for the operator.
    pub summary: String,
    /// Evidence citations supporting the diagnosis.
    pub evidence: Vec<EvidenceRef>,
}

/// Reasons a provider-neutral reasoning artifact fails its core contract.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ContractError {
    /// A citation does not exist on the current run-scoped evidence board.
    #[error("diagnostic report cites unknown evidence id: {0}")]
    UnknownEvidence(String),
    /// The diagnosis contains no meaningful summary text.
    #[error("diagnostic report must contain a non-empty summary")]
    EmptySummary,
    /// The diagnosis contains no evidence citations.
    #[error("diagnostic report must cite at least one evidence item")]
    MissingEvidence,
}

impl DiagnosticReport {
    /// Validates the report against the immutable evidence board for this run.
    pub fn validate_against(
        &self,
        available_evidence_ids: &BTreeSet<String>,
    ) -> Result<(), ContractError> {
        if self.summary.trim().is_empty() {
            return Err(ContractError::EmptySummary);
        }

        if self.evidence.is_empty() {
            return Err(ContractError::MissingEvidence);
        }

        self.evidence
            .iter()
            .find_map(|reference| {
                (!available_evidence_ids.contains(&reference.evidence_id))
                    .then(|| ContractError::UnknownEvidence(reference.evidence_id.clone()))
            })
            .map_or(Ok(()), Err)
    }
}
