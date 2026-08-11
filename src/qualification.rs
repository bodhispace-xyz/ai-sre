//! Deployment qualification contracts for external tools and providers.
//!
//! Qualification is intentionally fail-closed: runtime evidence must identify
//! the exact artifact and prove that self-update is disabled before a GCX
//! installation can be admitted by an operator.

use serde::Deserialize;

/// Evidence required to admit the reviewed GCX executable.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct GcxQualification {
    /// SHA-256 digest of the pinned executable bytes.
    pub binary_sha256: String,
    /// Digest of the image or runtime that executes the binary.
    pub runtime_digest: String,
    /// Digest identifying the reviewed software bill of materials.
    pub sbom_digest: String,
    /// Whether the deployment disables GCX self-update behavior.
    pub self_update_disabled: bool,
}

impl GcxQualification {
    /// Returns true only when every required deployment proof is present.
    pub fn is_accepted(&self) -> bool {
        !self.binary_sha256.trim().is_empty()
            && !self.runtime_digest.trim().is_empty()
            && !self.sbom_digest.trim().is_empty()
            && self.self_update_disabled
    }
}

#[cfg(test)]
mod tests {
    use super::GcxQualification;

    #[test]
    fn gcx_qualification_requires_complete_immutable_evidence() {
        // Given a qualification record for one reviewed GCX deployment.
        let qualification = GcxQualification {
            binary_sha256: "sha256:binary".to_owned(),
            runtime_digest: "sha256:runtime".to_owned(),
            sbom_digest: "sha256:sbom".to_owned(),
            self_update_disabled: true,
        };

        // When the admission predicate evaluates the record.
        let accepted = qualification.is_accepted();

        // Then only complete, immutable evidence is admitted.
        assert!(accepted);
    }

    #[test]
    fn gcx_qualification_rejects_missing_evidence_or_update_controls() {
        // Given a record missing the binary proof and allowing self-update.
        let qualification = GcxQualification {
            binary_sha256: String::new(),
            runtime_digest: "sha256:runtime".to_owned(),
            sbom_digest: "sha256:sbom".to_owned(),
            self_update_disabled: false,
        };

        // When the admission predicate evaluates the record.
        let accepted = qualification.is_accepted();

        // Then the deployment fails closed.
        assert!(!accepted);
    }
}
