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

/// Per-investigation allowance for read-only Grafana requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    remaining_queries: usize,
}

impl ContextBudget {
    /// Creates a budget that permits at most `max_queries` requests.
    pub const fn new(max_queries: usize) -> Self {
        Self {
            remaining_queries: max_queries,
        }
    }

    fn consume(&mut self) -> Result<(), ContextError> {
        let Some(remaining) = self.remaining_queries.checked_sub(1) else {
            return Err(ContextError::QueryBudgetExceeded);
        };
        self.remaining_queries = remaining;
        Ok(())
    }
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

    /// Executes a LogQL query after consuming one per-investigation budget token.
    pub async fn logs_with_budget(
        &self,
        board: &mut EvidenceBoard,
        budget: &mut ContextBudget,
        expression: impl Into<String>,
    ) -> Result<String, ContextError> {
        budget.consume()?;
        self.logs(board, expression).await
    }

    /// Executes a PromQL query after consuming one per-investigation budget token.
    pub async fn metrics_with_budget(
        &self,
        board: &mut EvidenceBoard,
        budget: &mut ContextBudget,
        expression: impl Into<String>,
    ) -> Result<String, ContextError> {
        budget.consume()?;
        self.metrics(board, expression).await
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
    /// The investigation exhausted its bounded read-only query allowance.
    #[error("Grafana context query budget exhausted")]
    QueryBudgetExceeded,
}

#[cfg(test)]
mod tests {
    use super::{ContextBudget, ContextError};

    #[test]
    fn context_budget_is_exhausted_without_underflow() {
        // Given a per-investigation budget containing one query.
        let mut budget = ContextBudget::new(1);

        // When the first query consumes the allowance and a second is attempted.
        let first = budget.consume();
        let second = budget.consume();

        // Then the first succeeds and exhaustion fails closed.
        assert_eq!(first, Ok(()));
        assert_eq!(second, Err(ContextError::QueryBudgetExceeded));
    }
}
