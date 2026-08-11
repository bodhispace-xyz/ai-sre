//! Gemini transport and response normalization at the adapter boundary.

use serde::Deserialize;

use crate::reasoning::contracts::{ContractError, DiagnosticReport};

use super::{ApiKey, ProviderFailure};

const DEFAULT_ENDPOINT: &str =
    "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent";

/// Minimal Gemini client configuration; the key is never exposed in logs.
#[derive(Debug, Clone)]
pub struct GeminiClient {
    _key: ApiKey,
    endpoint: String,
}

impl GeminiClient {
    /// Creates a client for the deployment-selected Gemini model endpoint.
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

    /// Converts Gemini's candidate text into the provider-neutral report.
    pub fn normalize_report(&self, response: &str) -> Result<DiagnosticReport, ProviderFailure> {
        let envelope = serde_json::from_str::<GeminiResponse>(response)
            .map_err(|_| ProviderFailure::MalformedResponse)?;
        let text = envelope
            .candidates
            .first()
            .and_then(|candidate| candidate.content.parts.first())
            .map(|part| part.text.as_str())
            .ok_or(ProviderFailure::MalformedResponse)?;
        DiagnosticReport::from_provider_json(text).map_err(contract_failure)
    }
}

#[derive(Debug, Deserialize)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: GeminiContent,
}

#[derive(Debug, Deserialize)]
struct GeminiContent {
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Deserialize)]
struct GeminiPart {
    text: String,
}

fn contract_failure(error: ContractError) -> ProviderFailure {
    match error {
        ContractError::MalformedProviderResponse => ProviderFailure::MalformedResponse,
        ContractError::UnknownEvidence(_)
        | ContractError::EmptySummary
        | ContractError::MissingEvidence => ProviderFailure::MalformedResponse,
    }
}
