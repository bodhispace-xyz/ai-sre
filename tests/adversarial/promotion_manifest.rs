//! GIVEN/WHEN/THEN boundary tests for untrusted promotion manifests.

use ai_sre::domain::promotion::PromotionManifest;

fn serialized_manifest() -> String {
    serde_json::json!({
        "schema":"KTD21/v1", "gate":"A", "manifest_digest":"sha256:bad",
        "predecessor_digest":null,
        "corpus_digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "results_digest":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "policy_version":"policy-1", "config_version":"config-1",
        "binary_digest":"sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        "issued_at":10, "expires_at":20, "key_id":"key-1", "generation":1, "revoked":false,
        "signature_verified":true
    })
    .to_string()
}

#[test]
fn serialized_input_cannot_claim_verification() {
    // Given a serialized manifest that attempts to add a trust flag.
    let parsed: Result<PromotionManifest, _> = serde_json::from_str(&serialized_manifest());
    // When the untrusted shape is validated.
    let result = parsed;
    // Then the forged identity is rejected and no verified type can be made.
    assert!(result.is_err());
}
