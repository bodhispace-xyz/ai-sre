//! Runtime admission policy for offline-signed promotion manifests.

use crate::domain::promotion::{Gate, PromotionError, PromotionManifest};

/// Values pinned by the running image and deployment policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeBinding {
    /// Current binary/image digest.
    pub binary_digest: String,
    /// Current policy version.
    pub policy_version: String,
    /// Current non-secret configuration version.
    pub config_version: String,
    /// Current replay corpus digest.
    pub corpus_digest: String,
    /// Trust-root generation accepted by this image.
    pub generation: u64,
    /// Key IDs revoked by the operator.
    pub revoked_key_ids: Vec<String>,
}

/// Verifies a manifest before it can be recorded as eligible.
pub fn admit(
    manifest: &PromotionManifest,
    binding: &RuntimeBinding,
    now: i64,
    expected_predecessor: Option<&str>,
) -> Result<(), PromotionError> {
    manifest.validate_shape()?;
    if !manifest.signature_verified
        || manifest.revoked
        || binding
            .revoked_key_ids
            .iter()
            .any(|key| key == &manifest.key_id)
    {
        return Err(PromotionError::UntrustedSignature);
    }
    if now < manifest.issued_at || now >= manifest.expires_at {
        return Err(PromotionError::NotCurrent);
    }
    if manifest.binary_digest != binding.binary_digest
        || manifest.policy_version != binding.policy_version
        || manifest.config_version != binding.config_version
        || manifest.corpus_digest != binding.corpus_digest
        || manifest.generation != binding.generation
    {
        return Err(PromotionError::BindingMismatch);
    }
    match (manifest.gate, expected_predecessor) {
        (Gate::A, None) => Ok(()),
        (Gate::A, Some(_)) => Err(PromotionError::InvalidPredecessor),
        (_, Some(previous)) if manifest.predecessor_digest.as_deref() == Some(previous) => Ok(()),
        _ => Err(PromotionError::InvalidPredecessor),
    }
}

/// Returns whether the service must remain in shadow mode.
pub fn shadow_only(gate_a: Result<(), PromotionError>) -> bool {
    gate_a.is_err()
}
