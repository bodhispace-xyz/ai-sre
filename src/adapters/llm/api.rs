//! Shared credential and failure primitives for API-backed model adapters.
//!
//! Vendor response parsing remains in each adapter; only safe classifications
//! and redacted credentials are shared here.

use std::fmt;

/// An API key that never appears in debug output.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// Creates a key from deployment-projected secret material.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(<redacted>)")
    }
}

/// Safe classifications crossing from a provider adapter into core logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFailure {
    /// The provider rejected credentials or the request was unauthorized.
    AuthenticationRequired,
    /// The provider limited the request rate or quota.
    RateLimited,
    /// The provider or transport was temporarily unavailable.
    TemporarilyUnavailable,
    /// The provider response did not match the expected typed contract.
    MalformedResponse,
}
