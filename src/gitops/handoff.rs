//! Defines validated operator artifacts without granting publication, merge, or deployment authority.

use super::{artifact::ManualRepairCandidate, receipt::digest, sandbox::ValidationReceipt};
use crate::reasoning::journal::JournalContext;

/// Deployment-owned freshness limit for importing successful sandbox results.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffPolicy {
    /// Maximum age in seconds, from 1 through 3600; future results are always rejected.
    pub max_validation_age_seconds: u64,
    /// Exact substantive validator image enrolled by deployment configuration, never learned from a result.
    pub validator_image: String,
    /// Independently enrolled protected runtime binary identity.
    pub runtime_digest: String,
    /// Exact enrolled sandbox limits; a different allowed policy still requires reenrollment.
    pub sandbox_limits: super::sandbox::SandboxLimits,
}

/// Server-owned state checked again immediately before durable operator handoff.
pub struct ManualHandoffRequest<'a> {
    /// Candidate previously committed to this journal.
    pub candidate: &'a ManualRepairCandidate,
    /// Receipt created only by a successful validator with confirmed cleanup.
    pub validation: &'a ValidationReceipt,
    /// Original incident/run scope, never selected by a model.
    pub context: &'a JournalContext,
    /// Fresh repository observation; the application must supply it through its read-only adapter.
    pub current_base: &'a str,
    /// Current Unix second from the application clock.
    pub now: u64,
    /// Deployment-owned validation freshness policy.
    pub policy: &'a HandoffPolicy,
}

/// A journaled artifact for operator review, not an externally published PR.
pub struct ManualRepairHandoff {
    json: String,
}

impl ManualRepairHandoff {
    /// Stable identity of the complete artifact and its bound validation receipt.
    pub fn digest(&self) -> String {
        digest(self.json.as_bytes())
    }

    /// Bounded, secret-free patch and metadata suitable for operator delivery.
    pub fn json(&self) -> &str {
        &self.json
    }

    pub(crate) fn build(request: &ManualHandoffRequest<'_>) -> Option<Self> {
        if !(1..=3600).contains(&request.policy.max_validation_age_seconds)
            || super::sandbox::SandboxPlan::new(
                &request.policy.validator_image,
                request.policy.sandbox_limits.clone(),
            )
            .is_err()
            || !request.validation.matches_enrollment(
                &request.policy.validator_image,
                &request.policy.runtime_digest,
                &request.policy.sandbox_limits,
            )
            || request.now.checked_sub(request.validation.completed_at())?
                > request.policy.max_validation_age_seconds
            || request.candidate.artifact_digest() != request.validation.artifact_digest()
            || !request
                .candidate
                .binds_handoff(request.context, request.current_base)
        {
            return None;
        }
        let mut wire: serde_json::Value = serde_json::from_str(&request.candidate.json()).ok()?;
        wire["schema"] = "ai-sre/manual-repair-handoff/v1".into();
        wire["candidate_digest"] = request.candidate.artifact_digest().into();
        wire["validation"] = serde_json::to_value(request.validation).ok()?;
        wire["status"] = "operator_review_required".into();
        wire["remote_checks"] = "not_run".into();
        wire["max_validation_age_seconds"] = request.policy.max_validation_age_seconds.into();
        // The original candidate stays unchanged in storage; this artifact reports the later validation.
        wire["description"] = request
            .candidate
            .description()
            .replace(
                "Manual repair candidate — not publish-ready.",
                "Validated repair artifact — operator review required.",
            )
            .replace(
                "Sandbox validation: not run.",
                "Sandbox validation: passed for the bound snapshot.",
            )
            .into();
        Some(Self {
            json: serde_json::to_string(&wire).ok()?,
        })
    }
}
