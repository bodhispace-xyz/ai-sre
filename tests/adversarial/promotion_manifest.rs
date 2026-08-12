//! Adversarial tests for fail-closed Gate A/B/C manifest admission.

use ai_sre::{
    domain::promotion::{Gate, PromotionError, PromotionManifest},
    policy::promotion::{RuntimeBinding, admit},
};

fn manifest(gate: Gate) -> PromotionManifest {
    PromotionManifest {
        schema: "KTD21/v1".into(),
        gate,
        manifest_digest: "sha256:m".into(),
        predecessor_digest: (gate != Gate::A).then(|| "sha256:previous".into()),
        corpus_digest: "sha256:c".into(),
        results_digest: "sha256:r".into(),
        policy_version: "policy-1".into(),
        config_version: "config-1".into(),
        binary_digest: "sha256:image".into(),
        issued_at: 10,
        expires_at: 20,
        key_id: "key-1".into(),
        generation: 1,
        revoked: false,
        signature_verified: true,
    }
}

fn binding() -> RuntimeBinding {
    RuntimeBinding {
        binary_digest: "sha256:image".into(),
        policy_version: "policy-1".into(),
        config_version: "config-1".into(),
        corpus_digest: "sha256:c".into(),
        generation: 1,
        revoked_key_ids: vec![],
    }
}

#[test]
fn unverified_or_expired_manifests_never_admit() {
    // Given a valid-looking Gate A manifest with no trusted signature.
    let mut candidate = manifest(Gate::A);
    candidate.signature_verified = false;
    // When the service evaluates it after expiry.
    let result = admit(&candidate, &binding(), 25, None);
    // Then no authority is granted, regardless of failure reason ordering.
    assert!(matches!(result, Err(PromotionError::UntrustedSignature)));
}

#[test]
fn later_gate_requires_exact_predecessor_and_runtime_bindings() {
    // Given a Gate B manifest and the exact Gate A digest.
    let candidate = manifest(Gate::B);
    // When the predecessor is wrong.
    let result = admit(&candidate, &binding(), 15, Some("sha256:not-a"));
    // Then the promotion chain fails closed.
    assert_eq!(result, Err(PromotionError::InvalidPredecessor));
}
