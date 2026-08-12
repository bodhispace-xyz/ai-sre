//! Versioned, bounded prompt fragments for advisory reasoning roles.

/// Prompt contract version included in every provider request.
pub const PROMPT_VERSION: &str = "u4-v1";
/// Maximum rendered role prompt retained before provider dispatch.
pub const MAX_ROLE_PROMPT_BYTES: usize = 32 * 1024;

/// Bounded prompt-rendering failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PromptError {
    /// The rendered prompt exceeded the provider input contract.
    #[error("role prompt exceeds its configured bound")]
    Oversized,
}

/// Returns the fixed instruction for one role.
pub const fn role_instruction(role: super::roles::ReasoningRole) -> &'static str {
    match role {
        super::roles::ReasoningRole::Investigator => {
            "Request only bounded read-only evidence; do not diagnose from missing data."
        }
        super::roles::ReasoningRole::Diagnostician => {
            "Rank hypotheses using cited immutable evidence and state uncertainty."
        }
        super::roles::ReasoningRole::Planner => {
            "Propose an advisory next step with explicit scope, verification, and stop conditions."
        }
        super::roles::ReasoningRole::Critic => {
            "Challenge citations, scope, expected impact, rollback, and unsafe assumptions."
        }
    }
}

/// Renders one role prompt from deterministic incident and evidence context.
pub fn render_role_prompt(
    role: super::roles::ReasoningRole,
    incident_id: &str,
    alert_name: &str,
    evidence_context: &str,
) -> Result<String, PromptError> {
    let prompt = format!(
        "Prompt version: {PROMPT_VERSION}\nRole: {role:?}\nIncident: {incident_id}\nAlert: {alert_name}\nInstruction: {}\nEvidence:\n{evidence_context}",
        role_instruction(role)
    );
    if prompt.len() > MAX_ROLE_PROMPT_BYTES {
        return Err(PromptError::Oversized);
    }
    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use super::{MAX_ROLE_PROMPT_BYTES, PromptError, render_role_prompt};
    use crate::reasoning::roles::ReasoningRole;

    #[test]
    fn role_prompt_is_versioned_and_bounded() {
        // Given deterministic incident context and one role.
        let prompt = render_role_prompt(ReasoningRole::Critic, "inc-1", "ApiDown", "evidence-0001")
            .expect("bounded prompt");

        // When the provider prompt is rendered.
        // Then it carries the version and role without provider state.
        assert!(prompt.contains("u4-v1"));
        assert!(prompt.contains("Critic"));
    }

    #[test]
    fn oversized_evidence_context_is_rejected_before_provider_dispatch() {
        // Given evidence larger than the provider input contract.
        let evidence = "x".repeat(MAX_ROLE_PROMPT_BYTES);

        // When the role prompt is rendered.
        let result = render_role_prompt(ReasoningRole::Investigator, "inc-1", "ApiDown", &evidence);

        // Then provider dispatch cannot receive an unbounded prompt.
        assert_eq!(result, Err(PromptError::Oversized));
    }
}
