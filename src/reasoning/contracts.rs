use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub evidence_id: String,
    pub claim: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticReport {
    pub summary: String,
    pub evidence: Vec<EvidenceRef>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ContractError {
    #[error("diagnostic report cites unknown evidence id: {0}")]
    UnknownEvidence(String),
    #[error("diagnostic report must contain a non-empty summary")]
    EmptySummary,
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

        self.evidence
            .iter()
            .find_map(|reference| {
                (!available_evidence_ids.contains(&reference.evidence_id))
                    .then(|| ContractError::UnknownEvidence(reference.evidence_id.clone()))
            })
            .map_or(Ok(()), Err)
    }
}
