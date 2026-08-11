//! DeepSeek transport and response normalization at the adapter boundary.

use serde::Deserialize;

use crate::reasoning::contracts::{ContractError, DiagnosticReport};

use super::{ApiKey, ProviderFailure};

const DEFAULT_ENDPOINT: &str = "https://api.deepseek.com/chat/completions";

/// Minimal DeepSeek client configuration; the key is never exposed in logs.
#[derive(Debug, Clone)]
pub struct DeepSeekClient {
    _key: ApiKey,
    endpoint: String,
}

impl DeepSeekClient {
    /// Creates a client for the deployment-selected DeepSeek endpoint.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            _key: ApiKey::new(api_key),
            endpoint: DEFAULT_ENDPOINT.to_owned(),
        }
    }

    /// Returns the configured endpoint for the transport shell.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Converts DeepSeek's choice text into the provider-neutral report.
    pub fn normalize_report(&self, response: &str) -> Result<DiagnosticReport, ProviderFailure> {
        let envelope = serde_json::from_str::<DeepSeekResponse>(response)
            .map_err(|_| ProviderFailure::MalformedResponse)?;
        let text = envelope
            .choices
            .first()
            .map(|choice| choice.message.content.as_str())
            .ok_or(ProviderFailure::MalformedResponse)?;
        DiagnosticReport::from_provider_json(text).map_err(contract_failure)
    }
}

#[derive(Debug, Deserialize)]
struct DeepSeekResponse {
    choices: Vec<DeepSeekChoice>,
}

#[derive(Debug, Deserialize)]
struct DeepSeekChoice {
    message: DeepSeekMessage,
}

#[derive(Debug, Deserialize)]
struct DeepSeekMessage {
    content: String,
}

fn contract_failure(error: ContractError) -> ProviderFailure {
    match error {
        ContractError::MalformedProviderResponse
        | ContractError::UnknownEvidence(_)
        | ContractError::EmptySummary
        | ContractError::MissingEvidence => ProviderFailure::MalformedResponse,
    }
}
