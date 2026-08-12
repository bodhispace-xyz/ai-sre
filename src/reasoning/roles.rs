//! Pure, bounded reasoning-role sequencing for one provider run.
//!
//! Roles are artifacts in one process, not independent agents. The graph is
//! finite and non-recursive: an investigator may request bounded evidence,
//! then diagnostician, planner, and critic each run at most once.

use thiserror::Error;

/// The fixed advisory role sequence for one complete reasoning run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningRole {
    /// Requests and interprets bounded context evidence.
    Investigator,
    /// Ranks hypotheses using the immutable evidence board.
    Diagnostician,
    /// Produces a bounded recommendation without execution authority.
    Planner,
    /// Challenges evidence, scope, and stop conditions.
    Critic,
}

/// Finite role graph state for one provider attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleGraph {
    next: Option<ReasoningRole>,
}

/// Role-graph transition failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RoleError {
    /// The graph has no remaining role to execute.
    #[error("reasoning role graph is complete")]
    Complete,
}

impl RoleGraph {
    /// Creates a fresh graph beginning with the investigator.
    pub const fn new() -> Self {
        Self {
            next: Some(ReasoningRole::Investigator),
        }
    }

    /// Returns the next role and advances the graph exactly once.
    pub fn advance(&mut self) -> Result<ReasoningRole, RoleError> {
        let role = self.next.ok_or(RoleError::Complete)?;
        self.next = match role {
            ReasoningRole::Investigator => Some(ReasoningRole::Diagnostician),
            ReasoningRole::Diagnostician => Some(ReasoningRole::Planner),
            ReasoningRole::Planner => Some(ReasoningRole::Critic),
            ReasoningRole::Critic => None,
        };
        Ok(role)
    }

    /// Returns whether the graph has no remaining role.
    pub const fn is_complete(&self) -> bool {
        self.next.is_none()
    }
}

impl Default for RoleGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{ReasoningRole, RoleError, RoleGraph};

    #[test]
    fn role_graph_is_finite_and_non_recursive() {
        // Given a fresh provider reasoning run.
        let mut graph = RoleGraph::new();

        // When each role advances once.
        let roles = [
            graph.advance().expect("investigator"),
            graph.advance().expect("diagnostician"),
            graph.advance().expect("planner"),
            graph.advance().expect("critic"),
        ];

        // Then the sequence cannot recurse or run a fifth role.
        assert_eq!(
            roles,
            [
                ReasoningRole::Investigator,
                ReasoningRole::Diagnostician,
                ReasoningRole::Planner,
                ReasoningRole::Critic,
            ]
        );
        assert!(graph.is_complete());
        assert_eq!(graph.advance(), Err(RoleError::Complete));
    }
}
