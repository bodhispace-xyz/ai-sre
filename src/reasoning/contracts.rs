//! Provider-neutral advisory artifacts and their fail-closed validation.
//!
//! These contracts accept model output but grant no runtime authority. Evidence
//! references must resolve against the immutable evidence board for the run.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum provider report JSON accepted before deserialization.
pub const MAX_PROVIDER_REPORT_BYTES: usize = 64 * 1024;

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

/// Advisory recommendation produced by the planner role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvisoryRecommendation {
    /// Human-readable proposed outcome; not an executable action.
    pub summary: String,
    /// Expected effect if an operator performs the recommendation.
    pub expected_effect: String,
    /// Fixed measurements an operator should use for verification.
    pub verification: String,
    /// Conditions that require stopping instead of continuing.
    pub stop_strategy: String,
    /// Operator-controlled rollback or recovery path.
    pub rollback: String,
    /// Evidence supporting the recommendation.
    pub evidence: Vec<EvidenceRef>,
}

/// Reasons a provider-neutral reasoning artifact fails its core contract.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ContractError {
    /// The provider response cannot be decoded as the core report contract.
    #[error("provider response does not match the diagnostic contract")]
    MalformedProviderResponse,
    /// A citation does not exist on the current run-scoped evidence board.
    #[error("diagnostic report cites unknown evidence id: {0}")]
    UnknownEvidence(String),
    /// The diagnosis contains no meaningful summary text.
    #[error("diagnostic report must contain a non-empty summary")]
    EmptySummary,
    /// The diagnosis contains no evidence citations.
    #[error("diagnostic report must cite at least one evidence item")]
    MissingEvidence,
    /// A planner artifact omitted a required safety field.
    #[error("advisory recommendation is missing a safety field")]
    MissingSafetyField,
}

impl AdvisoryRecommendation {
    /// Validates the advisory recommendation against immutable evidence.
    pub fn validate_against(
        &self,
        available_evidence_ids: &BTreeSet<String>,
    ) -> Result<(), ContractError> {
        if [
            self.summary.as_str(),
            self.expected_effect.as_str(),
            self.verification.as_str(),
            self.stop_strategy.as_str(),
            self.rollback.as_str(),
        ]
        .iter()
        .any(|field| field.trim().is_empty())
        {
            return Err(ContractError::MissingSafetyField);
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

impl DiagnosticReport {
    /// Converts untrusted provider JSON into the provider-neutral report type.
    ///
    /// The parser deliberately erases provider-specific parse details so error
    /// messages cannot accidentally disclose response content or credentials.
    pub fn from_provider_json(input: &str) -> Result<Self, ContractError> {
        if input.len() > MAX_PROVIDER_REPORT_BYTES {
            return Err(ContractError::MalformedProviderResponse);
        }
        serde_json::from_str(input).map_err(|_| ContractError::MalformedProviderResponse)
    }

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
