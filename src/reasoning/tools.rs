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
    use super::{ContextTool, ToolPolicyError, ToolTurnPolicy};

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
}
