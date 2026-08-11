//! DeepSeek transport and response normalization at the adapter boundary.

use serde::{Deserialize, Serialize};
use std::time::Instant;

use crate::reasoning::contracts::{ContractError, DiagnosticReport};
use crate::reasoning::coordinator::{AttemptFacts, FailureClass};
use crate::reasoning::live::{LiveCompletion, LiveProvider, LiveTurn};

use super::{ApiKey, ProviderFailure, REQUEST_TIMEOUT, bounded_response_body, classify_status};

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

        let body = bounded_response_body(response).await?;
        let body = std::str::from_utf8(&body).map_err(|_| ProviderFailure::MalformedResponse)?;
        self.normalize_report(body)
    }

    /// Executes one DeepSeek attempt in the core runtime result shape.
    pub async fn complete_for_runtime(
        &self,
        prompt: &str,
    ) -> Result<(DiagnosticReport, AttemptFacts), FailureClass> {
        let started = Instant::now();
        let report = self.complete(prompt).await.map_err(map_failure)?;
        Ok((
            report,
            AttemptFacts {
                elapsed_ms: started.elapsed().as_millis() as u64,
                ..AttemptFacts::default()
            },
        ))
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

impl LiveProvider for DeepSeekClient {
    fn complete<'a>(&'a self, prompt: &'a str) -> LiveCompletion<'a> {
        Box::pin(async move {
            self.complete_for_runtime(prompt)
                .await
                .map(|(report, facts)| LiveTurn::Final {
                    report,
                    tokens: facts.tokens,
                })
        })
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

fn map_failure(failure: ProviderFailure) -> FailureClass {
    match failure {
        ProviderFailure::AuthenticationRequired => FailureClass::AuthenticationRequired,
        ProviderFailure::RateLimited => FailureClass::RateLimited,
        ProviderFailure::TemporarilyUnavailable => FailureClass::TemporarilyUnavailable,
        ProviderFailure::MalformedResponse => FailureClass::MalformedResponse,
    }
}
