//! Versioned, secret-free application configuration and startup validation.
//!
//! Credentials are injected separately; this file contains only policy,
//! resource limits, paths, provider order, and datasource identifiers.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::reasoning::{budget::BudgetConfig, coordinator::ReasoningConfig, router::ProviderOrder};

/// Complete non-secret configuration required to assemble the application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    /// Reasoning provider order and incident budgets.
    pub reasoning: ReasoningConfig,
    /// Grafana read-only context settings.
    pub grafana: GrafanaConfig,
    /// OpenAI rotating refresh-token cache path.
    pub openai_cache_path: PathBuf,
    /// Pinned `gcx` executable path.
    pub gcx_binary: PathBuf,
    /// Maximum `gcx` process duration in seconds.
    pub gcx_timeout_secs: u64,
    /// Maximum stdout/stderr bytes accepted from `gcx`.
    pub gcx_max_output_bytes: usize,
    /// Maximum UTF-8 query expression bytes accepted by policy.
    pub gcx_max_query_bytes: usize,
    /// Maximum concurrent read-only `gcx` processes.
    pub gcx_max_concurrency: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            reasoning: ReasoningConfig::default(),
            grafana: GrafanaConfig::default(),
            openai_cache_path: PathBuf::from("/var/lib/ai-sre/openai/auth.json"),
            gcx_binary: PathBuf::from("/usr/local/bin/gcx"),
            gcx_timeout_secs: 30,
            gcx_max_output_bytes: 1_048_576,
            gcx_max_query_bytes: 4_096,
            gcx_max_concurrency: 4,
        }
    }
}

/// Fixed Grafana datasource identifiers used by read-only queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrafanaConfig {
    /// Loki datasource UID or alias.
    pub logs_datasource: String,
    /// Prometheus datasource UID or alias.
    pub metrics_datasource: String,
}

impl Default for GrafanaConfig {
    fn default() -> Self {
        Self {
            logs_datasource: "loki".to_owned(),
            metrics_datasource: "prometheus".to_owned(),
        }
    }
}

/// Startup validation failures that must stop the service before I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ConfigError {
    /// A required identifier or path is empty.
    #[error("required configuration value is empty")]
    EmptyValue,
    /// A process resource bound is zero and would not provide a safe limit.
    #[error("process resource limit must be greater than zero")]
    ZeroLimit,
    /// The pinned child executable must not depend on the caller's working directory.
    #[error("gcx binary path must be absolute")]
    RelativeGcxPath,
    /// The provider order is invalid.
    #[error("provider order is invalid")]
    InvalidProviderOrder,
}

/// Configuration file loading failures that prevent startup.
#[derive(Debug, Error)]
pub enum ConfigLoadError {
    /// The configuration file could not be read.
    #[error("configuration file could not be read")]
    Io(#[from] std::io::Error),
    /// The configuration file was not valid JSON or policy.
    #[error("configuration file is invalid")]
    Parse(#[from] serde_json::Error),
    /// The parsed configuration failed safety validation.
    #[error("configuration file failed validation")]
    Validation(#[from] ConfigError),
}

impl AppConfig {
    /// Parses and validates a complete secret-free JSON configuration.
    pub fn from_json(input: &str) -> Result<Self, ConfigLoadError> {
        let config: Self = serde_json::from_str(input)?;
        config.validate()?;
        Ok(config)
    }

    /// Loads and validates a complete JSON configuration from disk.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ConfigLoadError> {
        Self::from_json(&fs::read_to_string(path)?)
    }

    /// Rejects unsafe values before adapters or credentials are assembled.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.openai_cache_path.as_os_str().is_empty()
            || self.gcx_binary.as_os_str().is_empty()
            || self.grafana.logs_datasource.trim().is_empty()
            || self.grafana.metrics_datasource.trim().is_empty()
        {
            return Err(ConfigError::EmptyValue);
        }
        if !self.gcx_binary.is_absolute() {
            return Err(ConfigError::RelativeGcxPath);
        }
        if self.gcx_timeout_secs == 0
            || self.gcx_max_output_bytes == 0
            || self.gcx_max_query_bytes == 0
            || self.gcx_max_concurrency == 0
        {
            return Err(ConfigError::ZeroLimit);
        }
        self.reasoning
            .provider_order
            .validate()
            .map_err(|_| ConfigError::InvalidProviderOrder)
    }
}

/// Convenience constructor for deployments that override only policy values.
pub fn reasoning_config(order: ProviderOrder, budget: BudgetConfig) -> ReasoningConfig {
    ReasoningConfig {
        provider_order: order,
        budget,
        max_tool_turns: 4,
    }
}
