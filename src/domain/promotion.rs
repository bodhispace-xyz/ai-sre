//! Versioned promotion-manifest domain types.
//!
//! A manifest is evidence of an offline decision; it is never a substitute
//! for signature verification. The policy module accepts it only after the
//! deployment verifier has established that the signature is valid.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The three ordered promotion gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Gate {
    /// Shadow quality and deterministic fallback gate.
    A,
    /// Supervised mutation readiness gate.
    B,
    /// External GitHub proposal-write gate.
    C,
}

/// Canonical, signed promotion-manifest payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionManifest {
    /// Canonical schema identifier.
    pub schema: String,
    /// Gate represented by this manifest.
    pub gate: Gate,
    /// Digest of the canonical payload and result bundle.
    pub manifest_digest: String,
    /// Digest of the immediately preceding gate, when one exists.
    pub predecessor_digest: Option<String>,
    /// Frozen replay corpus digest.
    pub corpus_digest: String,
    /// Digest of measured gate results.
    pub results_digest: String,
    /// Version of the policy evaluated by the operator.
    pub policy_version: String,
    /// Version of the non-secret deployment configuration.
    pub config_version: String,
    /// Immutable application image or binary digest.
    pub binary_digest: String,
    /// Unix timestamp at which the manifest became valid.
    pub issued_at: i64,
    /// Unix timestamp after which the manifest must not grant authority.
    pub expires_at: i64,
    /// Offline signing key identifier.
    pub key_id: String,
    /// Monotonically increasing trust-root generation.
    pub generation: u64,
    /// Revocation is explicit and fail-closed.
    pub revoked: bool,
    /// Set only by the pinned offline verifier, never by deserialization.
    #[serde(skip)]
    pub signature_verified: bool,
}

/// Errors returned when a manifest cannot be admitted.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PromotionError {
    /// A field is empty or has the wrong shape.
    #[error("promotion manifest has invalid fields")]
    InvalidFields,
    /// The manifest is not valid at the supplied clock time.
    #[error("promotion manifest is expired or not yet valid")]
    NotCurrent,
    /// The signer has been revoked or the signature was not verified.
    #[error("promotion manifest signature is not trusted")]
    UntrustedSignature,
    /// The manifest does not bind to the running deployment.
    #[error("promotion manifest does not match the running deployment")]
    BindingMismatch,
    /// A gate skipped an earlier required gate.
    #[error("promotion predecessor is missing or incorrect")]
    InvalidPredecessor,
}

impl PromotionManifest {
    /// Returns the canonical JSON payload covered by the offline signature.
    ///
    /// Signature metadata and the in-memory verification bit are deliberately
    /// excluded, so a deserialized document cannot self-authorize.
    pub fn canonical_payload(&self) -> Vec<u8> {
        #[derive(Serialize)]
        struct Payload<'a> {
            schema: &'a str,
            gate: Gate,
            manifest_digest: &'a str,
            predecessor_digest: Option<&'a str>,
            corpus_digest: &'a str,
            results_digest: &'a str,
            policy_version: &'a str,
            config_version: &'a str,
            binary_digest: &'a str,
            issued_at: i64,
            expires_at: i64,
            key_id: &'a str,
            generation: u64,
            revoked: bool,
        }
        serde_json::to_vec(&Payload {
            schema: &self.schema,
            gate: self.gate,
            manifest_digest: &self.manifest_digest,
            predecessor_digest: self.predecessor_digest.as_deref(),
            corpus_digest: &self.corpus_digest,
            results_digest: &self.results_digest,
            policy_version: &self.policy_version,
            config_version: &self.config_version,
            binary_digest: &self.binary_digest,
            issued_at: self.issued_at,
            expires_at: self.expires_at,
            key_id: &self.key_id,
            generation: self.generation,
            revoked: self.revoked,
        })
        .expect("promotion payload contains only serializable fields")
    }

    /// Validates shape and gate-independent invariants.
    pub fn validate_shape(&self) -> Result<(), PromotionError> {
        let nonempty = [
            &self.schema,
            &self.manifest_digest,
            &self.corpus_digest,
            &self.results_digest,
            &self.policy_version,
            &self.config_version,
            &self.binary_digest,
            &self.key_id,
        ];
        if self.schema != "KTD21/v1"
            || nonempty.iter().any(|value| value.trim().is_empty())
            || self.issued_at >= self.expires_at
            || self.generation == 0
            || !self.manifest_digest.starts_with("sha256:")
            || !self.corpus_digest.starts_with("sha256:")
            || !self.results_digest.starts_with("sha256:")
            || !self.binary_digest.starts_with("sha256:")
        {
            return Err(PromotionError::InvalidFields);
        }
        if self.gate == Gate::A && self.predecessor_digest.is_some() {
            return Err(PromotionError::InvalidPredecessor);
        }
        if self.gate != Gate::A && self.predecessor_digest.as_deref().is_none_or(str::is_empty) {
            return Err(PromotionError::InvalidPredecessor);
        }
        if self
            .predecessor_digest
            .as_deref()
            .is_some_and(|digest| !digest.starts_with("sha256:"))
        {
            return Err(PromotionError::InvalidPredecessor);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Gate, PromotionManifest};

    fn fixture() -> PromotionManifest {
        PromotionManifest {
            schema: "KTD21/v1".into(),
            gate: Gate::A,
            manifest_digest: "sha256:manifest".into(),
            predecessor_digest: None,
            corpus_digest: "sha256:corpus".into(),
            results_digest: "sha256:results".into(),
            policy_version: "policy-1".into(),
            config_version: "config-1".into(),
            binary_digest: "sha256:image".into(),
            issued_at: 1,
            expires_at: 2,
            key_id: "key-1".into(),
            generation: 1,
            revoked: false,
            signature_verified: true,
        }
    }

    #[test]
    fn canonical_payload_excludes_verification_state() {
        // Given a manifest that has already been verified in memory.
        let mut manifest = fixture();
        let signed = manifest.canonical_payload();

        // When deserialization creates a fresh manifest from its JSON form.
        manifest.signature_verified = false;
        let unsigned = manifest.canonical_payload();

        // Then signature bytes are independent of mutable trust state.
        assert_eq!(signed, unsigned);
        assert!(!String::from_utf8_lossy(&signed).contains("signature_verified"));
    }
}
