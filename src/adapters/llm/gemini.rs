//! Gemini transport and response normalization at the adapter boundary.

use serde::{Deserialize, Serialize};
use std::time::Instant;

use crate::reasoning::contracts::{ContractError, DiagnosticReport};
use crate::reasoning::coordinator::{AttemptFacts, FailureClass};

use super::{ApiKey, ProviderFailure, REQUEST_TIMEOUT, bounded_response_body, classify_status};

const DEFAULT_ENDPOINT: &str =
    "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent";

/// Minimal Gemini client configuration; the key is never exposed in logs.
#[derive(Debug, Clone)]
pub struct GeminiClient {
    _key: ApiKey,
    endpoint: String,
    client: reqwest::Client,
}

impl GeminiClient {
    /// Creates a client for the deployment-selected Gemini model endpoint.
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
            .header("x-goog-api-key", self._key.value())
            .json(&GeminiRequest {
                contents: vec![GeminiContentRequest {
                    parts: vec![GeminiPartRequest {
                        text: prompt.to_owned(),
                    }],
                }],
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

    /// Executes one Gemini attempt in the core runtime result shape.
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

#[derive(Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContentRequest>,
}

#[derive(Serialize)]
struct GeminiContentRequest {
    parts: Vec<GeminiPartRequest>,
}

#[derive(Serialize)]
struct GeminiPartRequest {
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

fn map_failure(failure: ProviderFailure) -> FailureClass {
    match failure {
        ProviderFailure::AuthenticationRequired => FailureClass::AuthenticationRequired,
        ProviderFailure::RateLimited => FailureClass::RateLimited,
        ProviderFailure::TemporarilyUnavailable => FailureClass::TemporarilyUnavailable,
        ProviderFailure::MalformedResponse => FailureClass::MalformedResponse,
    }
}
