//! Typed promotion-manifest values and the verifier-owned trust boundary.
//!
//! Deserialized manifests are untrusted data. Only a verifier-owned receipt
//! can produce `VerifiedPromotionManifest`, which is the only value accepted
//! by runtime admission policy.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Promotion gates: runtime mutation and GitOps independently follow shadow qualification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Gate {
    /// Shadow quality gate.
    A,
    /// Supervised mutation gate.
    B,
    /// GitHub proposal gate.
    C,
}

/// Untrusted wire representation of a promotion manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionManifest {
    /// Canonical schema identifier.
    pub schema: String,
    /// Gate represented by this manifest.
    pub gate: Gate,
    /// Digest of the canonical payload, derived outside that payload.
    pub manifest_digest: String,
    /// Digest of the immediately preceding verified gate.
    pub predecessor_digest: Option<String>,
    /// Frozen replay corpus digest.
    pub corpus_digest: String,
    /// Digest of measured gate results.
    pub results_digest: String,
    /// Version of evaluated policy.
    pub policy_version: String,
    /// Version of deployment configuration.
    pub config_version: String,
    /// Immutable image or binary digest.
    pub binary_digest: String,
    /// Unix issue time.
    pub issued_at: i64,
    /// Unix expiry time.
    pub expires_at: i64,
    /// Offline signing key identifier.
    pub key_id: String,
    /// Trust-root generation.
    pub generation: u64,
    /// Explicit revocation marker.
    pub revoked: bool,
}

/// Verifier-owned receipt. Its private field prevents callers from forging it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifierReceipt {
    verified: (),
}

impl VerifierReceipt {
    /// Creates a receipt only for an offline verifier implementation.
    #[allow(dead_code)]
    pub(crate) const fn verified() -> Self {
        Self { verified: () }
    }
}

/// A manifest whose signature and trust-root binding were verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPromotionManifest {
    manifest: PromotionManifest,
    receipt: VerifierReceipt,
}

impl VerifiedPromotionManifest {
    /// Constructs a verified value from a verifier-owned receipt.
    #[allow(dead_code)]
    pub(crate) fn from_receipt(manifest: PromotionManifest, receipt: VerifierReceipt) -> Self {
        Self { manifest, receipt }
    }

    /// Returns the verified manifest data for policy binding.
    pub fn manifest(&self) -> &PromotionManifest {
        &self.manifest
    }

    /// Returns the digest used to identify this admitted gate.
    pub fn digest(&self) -> &str {
        &self.manifest.manifest_digest
    }
}

/// Errors returned when a manifest cannot be admitted.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PromotionError {
    /// A field is empty or malformed.
    #[error("promotion manifest has invalid fields")]
    InvalidFields,
    /// The manifest is outside its validity interval.
    #[error("promotion manifest is expired or not yet valid")]
    NotCurrent,
    /// The signature, key, or revocation state is not trusted.
    #[error("promotion manifest signature is not trusted")]
    UntrustedSignature,
    /// The manifest does not match the running deployment.
    #[error("promotion manifest does not match the running deployment")]
    BindingMismatch,
    /// The gate chain is missing or skips its predecessor.
    #[error("promotion predecessor is missing or incorrect")]
    InvalidPredecessor,
}

impl PromotionManifest {
    /// Returns canonical JSON bytes excluding the derived manifest digest.
    pub fn canonical_payload(&self) -> Vec<u8> {
        #[derive(Serialize)]
        struct Payload<'a> {
            schema: &'a str,
            gate: Gate,
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
        .expect("canonical payload is serializable")
    }

    /// Computes the identity of the exact canonical payload.
    pub fn derived_digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_payload());
        let bytes = hasher.finalize();
        format!(
            "sha256:{}",
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    }

    /// Validates shape and the derived manifest identity.
    pub fn validate_shape(&self) -> Result<(), PromotionError> {
        let fields = [
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
            || fields.iter().any(|v| v.trim().is_empty())
            || self.issued_at >= self.expires_at
            || self.generation == 0
            || self.manifest_digest != self.derived_digest()
            || !valid_digest(&self.corpus_digest)
            || !valid_digest(&self.results_digest)
            || !valid_digest(&self.binary_digest)
        {
            return Err(PromotionError::InvalidFields);
        }
        match (self.gate, self.predecessor_digest.as_deref()) {
            (Gate::A, None) => Ok(()),
            (Gate::A, Some(_)) => Err(PromotionError::InvalidPredecessor),
            (_, Some(value)) if valid_digest(value) => Ok(()),
            _ => Err(PromotionError::InvalidPredecessor),
        }
    }
}

fn valid_digest(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
pub(crate) fn test_verified(manifest: PromotionManifest) -> VerifiedPromotionManifest {
    VerifiedPromotionManifest::from_receipt(manifest, VerifierReceipt::verified())
}

#[cfg(test)]
mod tests {
    use super::{Gate, PromotionManifest};

    fn fixture() -> PromotionManifest {
        let mut manifest = PromotionManifest {
            schema: "KTD21/v1".into(),
            gate: Gate::A,
            manifest_digest: String::new(),
            predecessor_digest: None,
            corpus_digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            results_digest:
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            policy_version: "policy-1".into(),
            config_version: "config-1".into(),
            binary_digest:
                "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into(),
            issued_at: 1,
            expires_at: 2,
            key_id: "key-1".into(),
            generation: 1,
            revoked: false,
        };
        manifest.manifest_digest = manifest.derived_digest();
        manifest
    }

    #[test]
    fn canonical_payload_excludes_derived_identity() {
        // Given a valid manifest whose identity is derived from canonical bytes.
        let manifest = fixture();
        // When canonical bytes are hashed.
        let digest = manifest.derived_digest();
        // Then the identity is stable and not self-referential.
        assert_eq!(digest, manifest.manifest_digest);
        assert!(
            !String::from_utf8_lossy(&manifest.canonical_payload()).contains("manifest_digest")
        );
    }
}
