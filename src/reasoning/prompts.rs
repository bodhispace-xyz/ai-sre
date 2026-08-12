//! Versioned, bounded prompt fragments for advisory reasoning roles.

/// Prompt contract version included in every provider request.
pub const PROMPT_VERSION: &str = "u4-v1";
/// Maximum rendered role prompt retained before provider dispatch.
pub const MAX_ROLE_PROMPT_BYTES: usize = 32 * 1024;

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
