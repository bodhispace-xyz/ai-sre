//! Secret-safe OpenAI OAuth refresh-session primitives.
//!
//! This module owns session credentials at the adapter boundary. Core reports
//! never receive access tokens, refresh tokens, or vendor-specific error text.

use std::{
    fmt,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rig_core::client::ProviderClient;
use rig_core::{
    client::CompletionClient,
    completion::{AssistantContent, CompletionModel},
};

use crate::reasoning::{contracts::DiagnosticReport, coordinator::FailureClass};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const DEFAULT_TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
const CHATGPT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

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

/// A short-lived access token and its rotated refresh token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshedTokens {
    /// Access token passed to the provider client, never journaled.
    pub access_token: String,
    /// Replacement refresh token, or the previous one when omitted by OpenAI.
    pub refresh_token: String,
    /// Optional UNIX expiry supplied by the provider.
    pub expires_at: Option<i64>,
}

/// Errors raised while reading or atomically replacing the OAuth cache.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CacheError {
    /// The cache file is malformed or does not contain a refresh token.
    #[error("OpenAI OAuth cache is invalid")]
    Invalid,
    /// The cache could not be read or replaced.
    #[error("OpenAI OAuth cache could not be accessed")]
    Io,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheRecord {
    refresh_token: String,
}

/// Atomic, secret-only persistence for the rotating OpenAI refresh token.
#[derive(Debug, Clone)]
pub struct AuthCache {
    path: PathBuf,
}

impl AuthCache {
    /// Creates a cache at the deployment-owned path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Loads the current refresh token without exposing file contents in errors.
    pub fn load(&self) -> Result<Option<RefreshSession>, CacheError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(CacheError::Io),
        };

        let record: CacheRecord =
            serde_json::from_slice(&bytes).map_err(|_| CacheError::Invalid)?;
        if record.refresh_token.trim().is_empty() {
            return Err(CacheError::Invalid);
        }
        Ok(Some(RefreshSession::new(record.refresh_token)))
    }

    /// Replaces the cache atomically so a crash cannot leave a partial token.
    pub fn store(&self, session: &RefreshSession) -> Result<(), CacheError> {
        let parent = self.path.parent().ok_or(CacheError::Io)?;
        fs::create_dir_all(parent).map_err(|_| CacheError::Io)?;
        set_private_permissions(parent, true)?;

        let temporary = parent.join(format!(
            ".{}.{}.tmp",
            self.path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("auth"),
            unique_suffix()
        ));
        let bytes = serde_json::to_vec(&CacheRecord {
            refresh_token: session.refresh_token.clone(),
        })
        .map_err(|_| CacheError::Invalid)?;

        let write_result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .map_err(|_| CacheError::Io)?;
            set_private_permissions(&temporary, false)?;
            file.write_all(&bytes).map_err(|_| CacheError::Io)?;
            file.sync_all().map_err(|_| CacheError::Io)?;
            fs::rename(&temporary, &self.path).map_err(|_| CacheError::Io)
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }
}

/// Performs OpenAI refresh-token rotation without enabling device login.
#[derive(Clone)]
pub struct OpenAiOAuth {
    client: reqwest::Client,
    token_endpoint: String,
}

impl fmt::Debug for OpenAiOAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiOAuth")
            .field("token_endpoint", &self.token_endpoint)
            .finish_non_exhaustive()
    }
}

impl OpenAiOAuth {
    /// Creates a production client using OpenAI's OAuth token endpoint.
    pub fn new() -> Self {
        Self::with_endpoint(DEFAULT_TOKEN_ENDPOINT)
    }

    /// Creates a client with an explicit endpoint for recorded contract tests.
    pub fn with_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            token_endpoint: endpoint.into(),
        }
    }

    /// Exchanges a refresh token and classifies failures without provider text.
    pub async fn refresh(
        &self,
        session: &RefreshSession,
    ) -> Result<RefreshedTokens, RefreshFailure> {
        let response = self
            .client
            .post(&self.token_endpoint)
            .form(&[
                ("client_id", CHATGPT_CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", session.refresh_token.as_str()),
                ("scope", "openid profile email"),
            ])
            .send()
            .await
            .map_err(|_| RefreshFailure::TemporarilyUnavailable)?;

        if response.status().is_success() {
            let body = response
                .json::<RefreshResponse>()
                .await
                .map_err(|_| RefreshFailure::TemporarilyUnavailable)?;
            return Ok(RefreshedTokens {
                access_token: body.access_token,
                refresh_token: body
                    .refresh_token
                    .unwrap_or_else(|| session.refresh_token.clone()),
                expires_at: body.expires_at,
            });
        }

        if matches!(response.status().as_u16(), 400 | 401) {
            return Err(RefreshFailure::ReauthenticationRequired);
        }
        Err(RefreshFailure::TemporarilyUnavailable)
    }
}

impl Default for OpenAiOAuth {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds a Rig ChatGPT client from a short-lived access token.
///
/// The service never asks Rig to own refresh persistence or interactive login;
/// [`AuthCache`] and [`OpenAiOAuth`] perform those operations at this boundary.
pub fn rig_client(
    access_token: impl Into<String>,
) -> Result<rig_core::providers::chatgpt::Client, rig_core::client::ProviderClientError> {
    rig_core::providers::chatgpt::Client::from_val(
        rig_core::providers::chatgpt::ChatGPTAuth::AccessToken {
            access_token: access_token.into(),
            account_id: None,
        },
    )
}

/// Executes one OpenAI reasoning request with a refreshed short-lived token.
///
/// Rig remains entirely inside this adapter; core receives only normalized
/// report JSON and safe failure classifications.
pub async fn complete_with_rig(
    access_token: impl Into<String>,
    prompt: &str,
) -> Result<(DiagnosticReport, Option<u64>), FailureClass> {
    let client = rig_client(access_token).map_err(|_| FailureClass::TemporarilyUnavailable)?;
    let model = client.completion_model(rig_core::providers::chatgpt::GPT_5_4);
    let request = model.completion_request(prompt).build();
    let response = model
        .completion(request)
        .await
        .map_err(classify_rig_failure)?;
    let json = response
        .choice
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let report =
        DiagnosticReport::from_provider_json(&json).map_err(|_| FailureClass::MalformedResponse)?;
    Ok((report, Some(response.usage.total_tokens)))
}

/// Refreshes the cached session, atomically persists rotation, then completes.
///
/// This is the unattended service path: it never invokes device flow and never
/// returns access or refresh token material to callers.
pub async fn refresh_and_complete(
    oauth: &OpenAiOAuth,
    cache: &AuthCache,
    prompt: &str,
) -> Result<(DiagnosticReport, Option<u64>), FailureClass> {
    let session = cache
        .load()
        .map_err(|error| match error {
            CacheError::Invalid => FailureClass::AuthenticationRequired,
            CacheError::Io => FailureClass::TemporarilyUnavailable,
        })?
        .ok_or(FailureClass::AuthenticationRequired)?;
    let refreshed = oauth
        .refresh(&session)
        .await
        .map_err(|failure| match failure {
            RefreshFailure::ReauthenticationRequired => FailureClass::AuthenticationRequired,
            RefreshFailure::TemporarilyUnavailable => FailureClass::TemporarilyUnavailable,
        })?;
    cache
        .store(&RefreshSession::new(refreshed.refresh_token))
        .map_err(|error| match error {
            CacheError::Invalid => FailureClass::AuthenticationRequired,
            CacheError::Io => FailureClass::TemporarilyUnavailable,
        })?;
    complete_with_rig(refreshed.access_token, prompt).await
}

fn classify_rig_failure(error: rig_core::completion::CompletionError) -> FailureClass {
    classify_rig_status(
        error
            .provider_response_status()
            .map(|status| status.as_u16()),
    )
}

/// Classifies a Rig HTTP status without retaining vendor response text.
pub fn classify_rig_status(status: Option<u16>) -> FailureClass {
    match status {
        Some(401 | 403) => FailureClass::AuthenticationRequired,
        Some(429) => FailureClass::RateLimited,
        _ => FailureClass::TemporarilyUnavailable,
    }
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn set_private_permissions(path: &Path, directory: bool) -> Result<(), CacheError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if directory { 0o700 } else { 0o600 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|_| CacheError::Io)?;
    }
    #[cfg(not(unix))]
    let _ = (path, directory);
    Ok(())
}
