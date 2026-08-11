//! Language-model adapters that convert vendor responses into core contracts.

mod api;

pub(crate) use api::classify_status;
/// Shared safe classifications for API-backed provider adapters.
pub use api::{ApiKey, ProviderFailure};
pub(crate) use api::{MAX_RESPONSE_BYTES, REQUEST_TIMEOUT};

/// Google Gemini API adapter.
pub mod gemini;

/// DeepSeek API adapter.
pub mod deepseek;

/// OpenAI ChatGPT OAuth session handling.
pub mod openai;
