//! Fail-closed validation for advisory role artifacts.

use std::collections::BTreeSet;

use super::{
    contracts::{ContractError, DiagnosticReport},
    prompts::MAX_ROLE_PROMPT_BYTES,
};

/// Validates a provider report and the bounded prompt size used to produce it.
pub fn validate_report(
    report: &DiagnosticReport,
    evidence_ids: &BTreeSet<String>,
    prompt: &str,
) -> Result<(), ContractError> {
    if prompt.len() > MAX_ROLE_PROMPT_BYTES {
        return Err(ContractError::MalformedProviderResponse);
    }
    report.validate_against(evidence_ids)
}
