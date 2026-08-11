//! Language-model adapters that convert vendor responses into core contracts.

mod api;

pub(crate) use api::classify_status;
/// Shared safe classifications for API-backed provider adapters.
pub use api::{ApiKey, ProviderFailure};
pub(crate) use api::{MAX_RESPONSE_BYTES, REQUEST_TIMEOUT};

/// Reads a provider response without buffering beyond the configured cap.
pub(crate) async fn bounded_response_body(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, ProviderFailure> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(ProviderFailure::MalformedResponse);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ProviderFailure::TemporarilyUnavailable)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(ProviderFailure::MalformedResponse);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Google Gemini API adapter.
pub mod gemini;

/// DeepSeek API adapter.
pub mod deepseek;

/// OpenAI ChatGPT OAuth session handling.
pub mod openai;
