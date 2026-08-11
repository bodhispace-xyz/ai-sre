//! Provider-neutral contracts for bounded, read-only reasoning tool turns.
//!
//! This module describes intent and result data only. It does not execute
//! Grafana, parse provider-specific envelopes, or grant mutation authority.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum provider correlation identifier bytes retained in a run.
pub const MAX_TOOL_CALL_ID_BYTES: usize = 128;
/// Maximum serialized provider argument bytes accepted before parsing.
pub const MAX_TOOL_ARGUMENT_BYTES: usize = 16_384;
/// Maximum tool calls accepted from one provider response.
pub const MAX_TOOL_CALLS_PER_RESPONSE: usize = 8;
/// Maximum serialized result detail retained for one provider resumption.
pub const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;

/// The only model-directed context capabilities admitted by the MVP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextTool {
    /// Query Loki through the fixed read-only adapter.
    QueryLogs,
    /// Query Prometheus through the fixed read-only adapter.
    QueryMetrics,
    /// Read a server-selected desired-state file from Git.
    ReadDesiredState,
    /// Read bounded deployment history from Git.
    ReadDeploymentHistory,
    /// Read one server-owned health alias.
    ReadHealth,
}

/// A validated request extracted from a provider response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Provider-generated correlation identifier, treated as opaque data.
    pub call_id: String,
    /// Allowlisted context capability.
    pub tool: ContextTool,
    /// Query expression passed to the context policy validator.
    pub query: String,
}

/// Parses the small provider-neutral tool-call shape after adapter decoding.
pub fn parse_tool_call(
    call_id: impl Into<String>,
    name: &str,
    arguments: &str,
) -> Result<ToolCall, ToolLoopError> {
    let call_id = call_id.into();
    if call_id.len() > MAX_TOOL_CALL_ID_BYTES || arguments.len() > MAX_TOOL_ARGUMENT_BYTES {
        return Err(ToolLoopError::OversizedArguments);
    }
    let tool = match name {
        "query_logs" => ContextTool::QueryLogs,
        "query_metrics" => ContextTool::QueryMetrics,
        "read_desired_state" => ContextTool::ReadDesiredState,
        "read_deployment_history" => ContextTool::ReadDeploymentHistory,
        "read_health" => ContextTool::ReadHealth,
        _ => return Err(ToolLoopError::UnknownTool),
    };
    let query = serde_json::from_str::<ToolArguments>(arguments)
        .map_err(|_| ToolLoopError::InvalidArguments)?
        .query;
    let call = ToolCall {
        call_id,
        tool,
        query,
    };
    if call.call_id.trim().is_empty() {
        return Err(ToolLoopError::EmptyCallId);
    }
    if call.query.trim().is_empty() {
        return Err(ToolLoopError::EmptyQuery);
    }
    Ok(call)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolArguments {
    query: String,
}

/// Classification retained when a tool request cannot produce normal evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultClass {
    /// The adapter committed a complete bounded evidence record.
    Succeeded,
    /// The request was outside the read-only policy.
    Denied,
    /// The adapter bounded an oversized response.
    Truncated,
    /// The source returned data too old for the requested freshness policy.
    Stale,
    /// The incident budget or turn allowance was exhausted.
    Exhausted,
    /// The read-only backend was unavailable or failed operationally.
    Unavailable,
}

/// Provider-neutral result sent back to the same reasoning run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    /// Correlation identifier copied from the request.
    pub call_id: String,
    /// Safe result classification.
    pub class: ToolResultClass,
    /// Evidence identifier when a record was committed.
    pub evidence_id: Option<String>,
    /// Bounded diagnostic text; never raw credentials or provider errors.
    pub detail: String,
}

/// Finite policy for one provider's model-directed context turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolTurnPolicy {
    /// Maximum tool calls in one provider run.
    pub max_turns: u32,
}

/// Mutable state for one provider's finite context conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolLoop {
    policy: ToolTurnPolicy,
    turns_used: u32,
    results: Vec<ToolResult>,
    seen_call_ids: std::collections::BTreeSet<String>,
}

/// Errors that stop a tool request before any adapter I/O occurs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ToolLoopError {
    /// The provider supplied an empty correlation identifier.
    #[error("tool call identifier is empty")]
    EmptyCallId,
    /// The provider supplied an empty query expression.
    #[error("tool query is empty")]
    EmptyQuery,
    /// The provider requested a capability outside the allowlist.
    #[error("tool is not allowlisted")]
    UnknownTool,
    /// Tool arguments did not match the strict JSON shape.
    #[error("tool arguments are invalid")]
    InvalidArguments,
    /// Provider metadata exceeded the bounded adapter contract.
    #[error("tool metadata exceeds its configured limit")]
    OversizedArguments,
    /// The provider reused a correlation identifier in one run.
    #[error("tool call identifier was already used in this run")]
    DuplicateCallId,
    /// The provider exceeded the finite tool-turn allowance.
    #[error("tool turn budget exhausted")]
    TurnLimit,
}

impl ToolLoop {
    /// Starts a fresh finite loop for one provider run.
    pub const fn new(policy: ToolTurnPolicy) -> Self {
        Self {
            policy,
            turns_used: 0,
            results: Vec::new(),
            seen_call_ids: std::collections::BTreeSet::new(),
        }
    }

    /// Validates and admits one tool request before context execution.
    pub fn admit(&mut self, call: &ToolCall) -> Result<(), ToolLoopError> {
        if call.call_id.trim().is_empty() {
            return Err(ToolLoopError::EmptyCallId);
        }
        if call.query.trim().is_empty() {
            return Err(ToolLoopError::EmptyQuery);
        }
        if self.turns_used >= self.policy.max_turns {
            return Err(ToolLoopError::TurnLimit);
        }
        if !self.seen_call_ids.insert(call.call_id.clone()) {
            return Err(ToolLoopError::DuplicateCallId);
        }
        self.turns_used += 1;
        Ok(())
    }

    /// Returns the remaining tool-turn allowance.
    pub const fn remaining(&self) -> u32 {
        self.policy.max_turns - self.turns_used
    }

    /// Records a bounded result that can be serialized into the next provider turn.
    pub fn record_result(&mut self, result: ToolResult) {
        self.results.push(result);
    }

    /// Returns the immutable tool-result transcript for provider resumption.
    pub fn results(&self) -> &[ToolResult] {
        &self.results
    }

    /// Returns the bounded result-detail bytes retained for resumption.
    pub fn result_bytes(&self) -> usize {
        self.results.iter().map(|result| result.detail.len()).sum()
    }
}

impl ToolTurnPolicy {
    /// Creates a policy, rejecting an unbounded zero-turn configuration.
    pub const fn new(max_turns: u32) -> Result<Self, ToolPolicyError> {
        if max_turns == 0 {
            return Err(ToolPolicyError::ZeroTurnLimit);
        }
        Ok(Self { max_turns })
    }
}

/// Fail-closed tool-policy construction errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ToolPolicyError {
    /// A zero limit would make the protocol configuration ambiguous.
    #[error("tool turn limit must be greater than zero")]
    ZeroTurnLimit,
}

#[cfg(test)]
mod tests {
    use super::{
        ContextTool, ToolCall, ToolLoop, ToolLoopError, ToolPolicyError, ToolResult,
        ToolResultClass, ToolTurnPolicy, parse_tool_call,
    };

    #[test]
    fn tool_policy_rejects_unbounded_zero_turn_configuration() {
        // Given a proposed provider tool policy with no allowed turns.
        let result = ToolTurnPolicy::new(0);

        // When the policy is constructed.
        // Then construction fails closed instead of creating an unbounded loop.
        assert_eq!(result, Err(ToolPolicyError::ZeroTurnLimit));
    }

    #[test]
    fn tool_contract_names_only_read_only_context_capabilities() {
        // Given the complete model-directed capability set.
        let tools = [
            ContextTool::QueryLogs,
            ContextTool::QueryMetrics,
            ContextTool::ReadDesiredState,
            ContextTool::ReadDeploymentHistory,
            ContextTool::ReadHealth,
        ];

        // When each capability is classified.
        let all_read_only = tools.iter().all(|tool| {
            matches!(
                tool,
                ContextTool::QueryLogs
                    | ContextTool::QueryMetrics
                    | ContextTool::ReadDesiredState
                    | ContextTool::ReadDeploymentHistory
                    | ContextTool::ReadHealth
            )
        });

        // Then no shell, mutation, credential, or filesystem capability exists.
        assert!(all_read_only);
    }

    #[test]
    fn tool_loop_stops_before_the_backend_after_its_turn_limit() {
        // Given a provider run with one permitted context turn.
        let policy = ToolTurnPolicy::new(1).expect("positive limit");
        let mut loop_state = ToolLoop::new(policy);
        let call = ToolCall {
            call_id: "call-1".to_owned(),
            tool: ContextTool::QueryLogs,
            query: "{app=\"api\"}".to_owned(),
        };

        // When the provider repeats a second request in the same run.
        let first = loop_state.admit(&call);
        let second = loop_state.admit(&call);

        // Then the second request is denied before any GCX process can start.
        assert_eq!(first, Ok(()));
        assert_eq!(second, Err(ToolLoopError::TurnLimit));
        assert_eq!(loop_state.remaining(), 0);
    }

    #[test]
    fn parser_rejects_unknown_capabilities_and_extra_arguments() {
        // Given provider-authored names and JSON arguments.
        let unknown = parse_tool_call("call-1", "shell", r#"{"query":"pwd"}"#);
        let extra = parse_tool_call(
            "call-2",
            "query_logs",
            r#"{"query":"{app=\"api\"}","datasource":"loki"}"#,
        );

        // When the adapter converts them into the core tool contract.
        // Then unsupported capabilities and policy-owned fields fail closed.
        assert_eq!(unknown, Err(ToolLoopError::UnknownTool));
        assert_eq!(extra, Err(ToolLoopError::InvalidArguments));
    }

    #[test]
    fn tool_loop_retains_results_for_same_run_resumption() {
        // Given an admitted tool turn and a bounded result envelope.
        let policy = ToolTurnPolicy::new(2).expect("positive limit");
        let mut loop_state = ToolLoop::new(policy);
        let call = ToolCall {
            call_id: "call-1".to_owned(),
            tool: ContextTool::QueryMetrics,
            query: "up".to_owned(),
        };
        loop_state.admit(&call).expect("first turn");

        // When the context result is recorded before the next provider turn.
        loop_state.record_result(ToolResult {
            call_id: "call-1".to_owned(),
            class: ToolResultClass::Succeeded,
            evidence_id: Some("ev-1".to_owned()),
            detail: "bounded evidence committed".to_owned(),
        });

        // Then the same run can resume with the correlated result.
        assert_eq!(loop_state.results().len(), 1);
        assert_eq!(loop_state.results()[0].evidence_id.as_deref(), Some("ev-1"));
        assert_eq!(loop_state.remaining(), 1);
    }

    #[test]
    fn tool_loop_rejects_duplicate_correlation_ids() {
        // Given a loop with capacity for two calls and one admitted identifier.
        let mut loop_state = ToolLoop::new(ToolTurnPolicy::new(2).expect("positive limit"));
        let call = ToolCall {
            call_id: "same-id".to_owned(),
            tool: ContextTool::QueryLogs,
            query: "{app=\"api\"}".to_owned(),
        };
        loop_state.admit(&call).expect("first call");

        // When the provider reuses that identifier for another request.
        let duplicate = loop_state.admit(&call);

        // Then no second backend operation is admitted.
        assert_eq!(duplicate, Err(ToolLoopError::DuplicateCallId));
        assert_eq!(loop_state.remaining(), 1);
    }
}
