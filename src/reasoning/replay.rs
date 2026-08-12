//! Deterministic replay identities for recorded U4 provider evaluations.

use sha2::{Digest, Sha256};

/// Versioned replay fixture identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayCase {
    /// Stable case name from the committed fixture manifest.
    pub name: String,
    /// Prompt/policy version used by the fixture.
    pub version: String,
    /// Digest of normalized input evidence.
    pub input_digest: String,
}

impl ReplayCase {
    /// Creates a stable replay identity from normalized input text.
    pub fn new(name: impl Into<String>, version: impl Into<String>, input: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        let digest = hasher.finalize();
        let input_digest = format!(
            "sha256:{}",
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        Self {
            name: name.into(),
            version: version.into(),
            input_digest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ReplayCase;

    #[test]
    fn replay_identity_is_stable_for_same_normalized_input() {
        // Given a recorded provider case and normalized evidence input.
        let first = ReplayCase::new("api-down", "u4-v1", "evidence-0001\nup");
        let second = ReplayCase::new("api-down", "u4-v1", "evidence-0001\nup");

        // When replay identities are computed.
        // Then the fixture can be compared without provider-specific state.
        assert_eq!(first, second);
        assert!(first.input_digest.starts_with("sha256:"));
    }
}
