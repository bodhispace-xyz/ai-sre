//! DeepSeek transport and response normalization at the adapter boundary.

use serde::{Deserialize, Serialize};

use crate::reasoning::contracts::{ContractError, DiagnosticReport};

use super::{ApiKey, MAX_RESPONSE_BYTES, ProviderFailure, REQUEST_TIMEOUT, classify_status};

const DEFAULT_ENDPOINT: &str = "https://api.deepseek.com/chat/completions";

/// Minimal DeepSeek client configuration; the key is never exposed in logs.
#[derive(Debug, Clone)]
pub struct DeepSeekClient {
    _key: ApiKey,
    endpoint: String,
    client: reqwest::Client,
}

impl DeepSeekClient {
    /// Creates a client for the deployment-selected DeepSeek endpoint.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            _key: ApiKey::new(api_key),
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("default HTTP client configuration must be valid"),
        }
    }

    /// Returns the configured endpoint for the transport shell.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Sends one bounded prompt and normalizes the provider response.
    pub async fn complete(&self, prompt: &str) -> Result<DiagnosticReport, ProviderFailure> {
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(self._key.value())
            .json(&DeepSeekRequest {
                model: "deepseek-chat",
                messages: vec![DeepSeekMessageRequest {
                    role: "user",
                    content: prompt,
                }],
                response_format: DeepSeekResponseFormat {
                    response_type: "json_object",
                },
            })
            .send()
            .await
            .map_err(|_| ProviderFailure::TemporarilyUnavailable)?;

        if !response.status().is_success() {
            return Err(classify_status(response.status()));
        }

        let body = response
            .bytes()
            .await
            .map_err(|_| ProviderFailure::TemporarilyUnavailable)?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(ProviderFailure::MalformedResponse);
        }
        let body = std::str::from_utf8(&body).map_err(|_| ProviderFailure::MalformedResponse)?;
        self.normalize_report(body)
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

#[derive(Serialize)]
struct DeepSeekRequest<'a> {
    model: &'a str,
    messages: Vec<DeepSeekMessageRequest<'a>>,
    response_format: DeepSeekResponseFormat<'a>,
}

#[derive(Serialize)]
struct DeepSeekMessageRequest<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct DeepSeekResponseFormat<'a> {
    #[serde(rename = "type")]
    response_type: &'a str,
}

fn contract_failure(error: ContractError) -> ProviderFailure {
    match error {
        ContractError::MalformedProviderResponse
        | ContractError::UnknownEvidence(_)
        | ContractError::EmptySummary
        | ContractError::MissingEvidence => ProviderFailure::MalformedResponse,
    }
}
