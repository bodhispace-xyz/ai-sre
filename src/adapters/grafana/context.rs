//! Read-only Grafana context assembly over the bounded `gcx` runner.
//!
//! This adapter converts tool output into core evidence records. It cannot
//! execute arbitrary commands, mutate Grafana, or expose credentials to core.

use crate::reasoning::evidence::{
    EvidenceBoard, EvidenceError, EvidenceMetadata, EvidenceSource, EvidenceStatus,
    MAX_EVIDENCE_QUERY_BYTES,
};
use crate::reasoning::tools::{ContextTool, ToolCall, ToolResult, ToolResultClass};

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

/// One model-directed, read-only context request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyRequest {
    /// Ask Loki for bounded logs through the configured datasource.
    Logs(String),
    /// Ask Prometheus for bounded metrics through the configured datasource.
    Metrics(String),
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

    /// Executes one validated model tool call and returns a safe result envelope.
    pub async fn execute_tool(
        &self,
        board: &mut EvidenceBoard,
        budget: &mut ContextBudget,
        call: &ToolCall,
    ) -> ToolResult {
        let result = match call.tool {
            ContextTool::QueryLogs => {
                self.logs_with_budget(board, budget, call.query.clone())
                    .await
            }
            ContextTool::QueryMetrics => {
                self.metrics_with_budget(board, budget, call.query.clone())
                    .await
            }
            ContextTool::ReadDesiredState
            | ContextTool::ReadDeploymentHistory
            | ContextTool::ReadHealth
            | ContextTool::DiscoverObservability => Err(ContextError::UnsupportedTool),
        };
        let mut result = match result {
            Ok(evidence_id) => ToolResult {
                call_id: call.call_id.clone(),
                class: ToolResultClass::Succeeded,
                evidence_id: Some(evidence_id),
                detail: "bounded evidence committed".to_owned(),
            },
            Err(ContextError::QueryBudgetExceeded) => ToolResult {
                call_id: call.call_id.clone(),
                class: ToolResultClass::Exhausted,
                evidence_id: None,
                detail: "context query budget exhausted".to_owned(),
            },
            Err(ContextError::Gcx(GcxRunError::OutputLimitExceeded)) => ToolResult {
                call_id: call.call_id.clone(),
                class: ToolResultClass::Truncated,
                evidence_id: None,
                detail: "context output exceeded its bound".to_owned(),
            },
            Err(ContextError::Gcx(GcxRunError::InvalidQuery)) => ToolResult {
                call_id: call.call_id.clone(),
                class: ToolResultClass::Denied,
                evidence_id: None,
                detail: "query rejected by context policy".to_owned(),
            },
            Err(ContextError::Gcx(_)) => ToolResult {
                call_id: call.call_id.clone(),
                class: ToolResultClass::Unavailable,
                evidence_id: None,
                detail: "context backend unavailable".to_owned(),
            },
            Err(_) => ToolResult {
                call_id: call.call_id.clone(),
                class: ToolResultClass::Denied,
                evidence_id: None,
                detail: "context query failed safely".to_owned(),
            },
        };
        if result.evidence_id.is_none() {
            let source = match call.tool {
                ContextTool::QueryLogs => EvidenceSource::GrafanaLogs,
                ContextTool::QueryMetrics => EvidenceSource::GrafanaMetrics,
                ContextTool::ReadDesiredState
                | ContextTool::ReadDeploymentHistory
                | ContextTool::ReadHealth
                | ContextTool::DiscoverObservability => EvidenceSource::ObservabilityMetadata,
            };
            result.evidence_id = board
                .commit_with_metadata(
                    source,
                    bounded_failure_query(&call.query),
                    b"[NO EVIDENCE]".to_vec(),
                    failure_metadata(result.class),
                )
                .ok();
        }
        result
    }

    /// Executes a bounded batch of model-directed read-only requests.
    pub async fn execute_requests(
        &self,
        board: &mut EvidenceBoard,
        budget: &mut ContextBudget,
        requests: &[ReadOnlyRequest],
    ) -> Result<Vec<String>, ContextError> {
        let mut evidence_ids = Vec::with_capacity(requests.len());
        for request in requests {
            let evidence_id = match request {
                ReadOnlyRequest::Logs(expression) => {
                    self.logs_with_budget(board, budget, expression.clone())
                        .await?
                }
                ReadOnlyRequest::Metrics(expression) => {
                    self.metrics_with_budget(board, budget, expression.clone())
                        .await?
                }
            };
            evidence_ids.push(evidence_id);
        }
        Ok(evidence_ids)
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

fn bounded_failure_query(query: &str) -> String {
    if query.len() <= MAX_EVIDENCE_QUERY_BYTES {
        query.to_owned()
    } else {
        "[QUERY_REDACTED]".to_owned()
    }
}

fn failure_metadata(class: ToolResultClass) -> EvidenceMetadata {
    let (status, truncated, error) = match class {
        ToolResultClass::Truncated => (
            EvidenceStatus::Partial,
            true,
            Some("output_limit_exceeded".to_owned()),
        ),
        ToolResultClass::Denied => (
            EvidenceStatus::Rejected,
            false,
            Some("query_rejected".to_owned()),
        ),
        ToolResultClass::Exhausted => (
            EvidenceStatus::BudgetExhausted,
            false,
            Some("budget_exhausted".to_owned()),
        ),
        ToolResultClass::Stale => (
            EvidenceStatus::Empty,
            false,
            Some("stale_source".to_owned()),
        ),
        ToolResultClass::Unavailable | ToolResultClass::Succeeded => (
            EvidenceStatus::Unavailable,
            false,
            Some("context_unavailable".to_owned()),
        ),
    };
    EvidenceMetadata {
        status,
        freshness_ms: None,
        truncated,
        error,
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
    /// The requested capability belongs to another read-only adapter.
    #[error("context capability is handled by another adapter")]
    UnsupportedTool,
}

#[cfg(test)]
mod tests {
    use super::{ContextBudget, ContextError, ReadOnlyRequest};

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

    #[test]
    fn read_only_requests_have_no_command_or_datasource_field() {
        // Given a model-directed request for additional evidence.
        let request = ReadOnlyRequest::Logs("{service=\"api\"} |= \"error\"".to_owned());

        // When its typed capability is inspected.
        let is_logs = matches!(request, ReadOnlyRequest::Logs(_));

        // Then the request contains only query data; command and datasource stay policy-owned.
        assert!(is_logs);
    }
}
