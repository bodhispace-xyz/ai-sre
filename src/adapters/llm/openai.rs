//! Secret-safe OpenAI OAuth refresh-session primitives.
//!
//! This module owns session credentials at the adapter boundary. Core reports
//! never receive access tokens, refresh tokens, or vendor-specific error text.

use std::fmt;

/// A refreshable OpenAI session without exposing its token in diagnostics.
#[derive(Clone, PartialEq, Eq)]
pub struct RefreshSession {
    refresh_token: String,
}

impl RefreshSession {
    /// Creates a session from a deployment-projected refresh token.
    pub fn new(refresh_token: impl Into<String>) -> Self {
        Self {
            refresh_token: refresh_token.into(),
        }
    }

    /// Replaces the session after a successful provider rotation.
    pub fn rotate(&mut self, next_refresh_token: impl Into<String>) {
        self.refresh_token = next_refresh_token.into();
    }
}

impl fmt::Debug for RefreshSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RefreshSession")
            .finish_non_exhaustive()
    }
}

/// Safe classifications for failed non-interactive refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshFailure {
    /// The provider rejected the refresh token and interactive login is needed.
    ReauthenticationRequired,
    /// The provider or transport was temporarily unavailable.
    TemporarilyUnavailable,
}
