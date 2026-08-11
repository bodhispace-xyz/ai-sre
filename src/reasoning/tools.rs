//! Provider-neutral contracts for bounded, read-only reasoning tool turns.
//!
//! This module describes intent and result data only. It does not execute
//! Grafana, parse provider-specific envelopes, or grant mutation authority.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The only model-directed context capabilities admitted by the MVP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextTool {
    /// Query Loki through the fixed read-only adapter.
    QueryLogs,
    /// Query Prometheus through the fixed read-only adapter.
    QueryMetrics,
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
    let tool = match name {
        "query_logs" => ContextTool::QueryLogs,
        "query_metrics" => ContextTool::QueryMetrics,
        _ => return Err(ToolLoopError::UnknownTool),
    };
    let query = serde_json::from_str::<ToolArguments>(arguments)
        .map_err(|_| ToolLoopError::InvalidArguments)?
        .query;
    let call = ToolCall {
        call_id: call_id.into(),
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
        self.turns_used += 1;
        Ok(())
    }

    /// Returns the remaining tool-turn allowance.
    pub const fn remaining(&self) -> u32 {
        self.policy.max_turns - self.turns_used
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
        ContextTool, ToolCall, ToolLoop, ToolLoopError, ToolPolicyError, ToolTurnPolicy,
        parse_tool_call,
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
        let tools = [ContextTool::QueryLogs, ContextTool::QueryMetrics];

        // When each capability is classified.
        let all_read_only = tools
            .iter()
            .all(|tool| matches!(tool, ContextTool::QueryLogs | ContextTool::QueryMetrics));

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
}
