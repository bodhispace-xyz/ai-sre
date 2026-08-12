//! Application assembly after configuration has passed fail-closed validation.
//!
//! Bootstrap owns dependency construction only; incident decisions remain in
//! the reasoning runtime and adapters retain all vendor/process boundaries.

use std::time::Duration;

use thiserror::Error;

use crate::{
    adapters::{
        context::{ReadOnlyContext, ReadOnlyContextConfig, ReadOnlyRunner},
        grafana::{
            context::{GrafanaContext, GrafanaContextConfig},
            gcx::GcxRunner,
        },
        llm::openai::{AuthCache, OpenAiOAuth},
    },
    config::{AppConfig, ConfigError},
    reasoning::{coordinator::ReasoningRun, runtime::IncidentRuntime},
};

/// Assembled dependencies for one process instance.
#[derive(Debug)]
pub struct Application {
    /// Runtime-owned incident coordinator, evidence board, and journal.
    pub runtime: IncidentRuntime,
    /// Read-only Grafana context adapter.
    pub grafana: GrafanaContext,
    /// Server-owned Git, deployment-history, and health context adapter.
    pub read_only: ReadOnlyContext,
    /// OpenAI refresh-token cache boundary.
    pub openai_cache: AuthCache,
    /// OpenAI OAuth transport boundary.
    pub openai_oauth: OpenAiOAuth,
}

/// Bootstrap failures before the application becomes available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum BootstrapError {
    /// Configuration failed validation.
    #[error("application configuration is invalid")]
    Configuration(#[from] ConfigError),
    /// Reasoning policy could not be assembled.
    #[error("reasoning runtime could not be assembled")]
    Reasoning,
}

/// Validates policy and assembles all non-secret runtime dependencies.
pub fn build(config: AppConfig) -> Result<Application, BootstrapError> {
    config.validate()?;
    let runtime = IncidentRuntime::new(config.reasoning).map_err(|_| BootstrapError::Reasoning)?;
    let grafana = GrafanaContext::new(
        GcxRunner::new(
            config.gcx_binary,
            Duration::from_secs(config.gcx_timeout_secs),
            config.gcx_max_output_bytes,
        )
        .with_max_query_bytes(config.gcx_max_query_bytes)
        .with_max_concurrency(config.gcx_max_concurrency),
        GrafanaContextConfig {
            logs_datasource: config.grafana.logs_datasource,
            metrics_datasource: config.grafana.metrics_datasource,
        },
    );
    let read_only = ReadOnlyContext::new(
        ReadOnlyRunner::new(
            Duration::from_secs(config.gcx_timeout_secs),
            config.gcx_max_output_bytes,
            config.gcx_max_concurrency,
        ),
        ReadOnlyContextConfig {
            git_binary: config.read_only.git_binary,
            git_repository: config.read_only.git_repository,
            health_binary: config.read_only.health_binary,
            health_commands: config.read_only.health_commands,
        },
    )
    .map_err(|_| BootstrapError::Configuration(ConfigError::RelativeReadOnlyPath))?;
    Ok(Application {
        runtime,
        grafana,
        read_only,
        openai_cache: AuthCache::new(config.openai_cache_path),
        openai_oauth: OpenAiOAuth::new(),
    })
}

/// Checks reasoning policy without constructing external clients.
pub fn validate_reasoning(config: &AppConfig) -> Result<(), BootstrapError> {
    ReasoningRun::new(config.reasoning.clone())
        .map(|_| ())
        .map_err(|_| BootstrapError::Reasoning)
}
