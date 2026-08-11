//! Read-only Grafana context assembly over the bounded `gcx` runner.
//!
//! This adapter converts tool output into core evidence records. It cannot
//! execute arbitrary commands, mutate Grafana, or expose credentials to core.

use crate::reasoning::evidence::{EvidenceBoard, EvidenceError, EvidenceSource};

use super::gcx::{GcxQuery, GcxRunError, GcxRunner};

/// Grafana datasource configuration for the two read-only context paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrafanaContextConfig {
    /// Loki datasource UID or alias.
    pub logs_datasource: String,
    /// Prometheus datasource UID or alias.
    pub metrics_datasource: String,
}

/// Read-only context service used by the reasoning shell.
#[derive(Debug, Clone)]
pub struct GrafanaContext {
    runner: GcxRunner,
    config: GrafanaContextConfig,
}

impl GrafanaContext {
    /// Creates a context service with fixed datasource policy.
    pub fn new(runner: GcxRunner, config: GrafanaContextConfig) -> Self {
        Self { runner, config }
    }

    /// Executes a bounded LogQL query and commits its output to the board.
    pub async fn logs(
        &self,
        board: &mut EvidenceBoard,
        expression: impl Into<String>,
    ) -> Result<String, ContextError> {
        self.query(
            board,
            GcxQuery::logs(expression, &self.config.logs_datasource),
            EvidenceSource::GrafanaLogs,
        )
        .await
    }

    /// Executes a bounded PromQL query and commits its output to the board.
    pub async fn metrics(
        &self,
        board: &mut EvidenceBoard,
        expression: impl Into<String>,
    ) -> Result<String, ContextError> {
        self.query(
            board,
            GcxQuery::metrics(expression, &self.config.metrics_datasource),
            EvidenceSource::GrafanaMetrics,
        )
        .await
    }

    async fn query(
        &self,
        board: &mut EvidenceBoard,
        query: GcxQuery,
        source: EvidenceSource,
    ) -> Result<String, ContextError> {
        let output = self.runner.run(&query).await.map_err(ContextError::Gcx)?;
        let expression = match &query {
            GcxQuery::Logs { expression, .. } | GcxQuery::Metrics { expression, .. } => expression,
        };
        board
            .commit(source, expression.clone(), output.stdout)
            .map_err(ContextError::Evidence)
    }
}

/// Safe failures at the Grafana context boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    /// The bounded process adapter rejected the query execution.
    #[error("Grafana context query failed")]
    Gcx(#[from] GcxRunError),
    /// The tool returned no commit-worthy evidence.
    #[error("Grafana context evidence could not be committed")]
    Evidence(#[from] EvidenceError),
}
