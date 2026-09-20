//! Runtime admission policy for verifier-backed promotion manifests.

use crate::domain::promotion::{Gate, PromotionError, VerifiedPromotionManifest};

/// Deployment-owned values pinned by the running image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeBinding {
    /// Current image digest.
    pub binary_digest: String,
    /// Current policy version.
    pub policy_version: String,
    /// Current configuration version.
    pub config_version: String,
    /// Current replay corpus digest.
    pub corpus_digest: String,
    /// Accepted trust-root generation.
    pub generation: u64,
    /// Revoked key identifiers.
    pub revoked_key_ids: Vec<String>,
}

/// Durable identity of a previously admitted gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedGate {
    /// Gate kind.
    pub gate: Gate,
    /// Verified identity.
    pub digest: String,
}

/// Version-controlled shadow-only Gate A policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowGatePolicy {
    /// Required corpus cases.
    pub minimum_cases: usize,
    /// Required failure classes.
    pub minimum_failure_classes: usize,
    /// Required repeated runs.
    pub repeats_per_case: usize,
    /// Action policy must remain disabled.
    pub action_policy_disabled: bool,
    /// External write credentials must be absent.
    pub external_write_credentials_absent: bool,
}

impl ShadowGatePolicy {
    /// Parses the deliberately flat, scalar Gate A YAML contract.
    pub fn from_yaml(input: &str) -> Result<Self, PromotionError> {
        let mut values = std::collections::BTreeMap::new();
        for line in input
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        {
            let (key, value) = line.split_once(':').ok_or(PromotionError::InvalidFields)?;
            values.insert(key.trim(), value.trim());
        }
        const ALLOWED: &[&str] = &[
            "schema",
            "gate",
            "mode",
            "minimum_cases",
            "minimum_failure_classes",
            "repeats_per_case",
            "p95_max_seconds",
            "max_evidence_rounds",
            "max_grafana_queries",
            "requires_deterministic_fallback",
            "requires_openai",
            "requires_gemini",
            "requires_deepseek",
            "requires_offline_signature",
            "action_policy",
            "external_write_credentials",
        ];
        if values.keys().any(|key| !ALLOWED.contains(key)) {
            return Err(PromotionError::InvalidFields);
        }
        let required = |key: &str| {
            values
                .get(key)
                .copied()
                .ok_or(PromotionError::InvalidFields)
        };
        if required("schema")? != "KTD21/v1"
            || required("gate")? != "A"
            || required("mode")? != "shadow"
            || required("requires_offline_signature")? != "true"
        {
            return Err(PromotionError::InvalidFields);
        }
        Ok(Self {
            minimum_cases: required("minimum_cases")?
                .parse()
                .map_err(|_| PromotionError::InvalidFields)?,
            minimum_failure_classes: required("minimum_failure_classes")?
                .parse()
                .map_err(|_| PromotionError::InvalidFields)?,
            repeats_per_case: required("repeats_per_case")?
                .parse()
                .map_err(|_| PromotionError::InvalidFields)?,
            action_policy_disabled: required("action_policy")? == "disabled",
            external_write_credentials_absent: required("external_write_credentials")? == "absent",
        })
    }

    /// Fails closed unless the corpus and shadow-only controls meet policy.
    pub fn admits_shadow(&self, case_count: usize, class_count: usize, repeats: usize) -> bool {
        self.minimum_cases <= case_count
            && self.minimum_failure_classes <= class_count
            && self.repeats_per_case <= repeats
            && self.action_policy_disabled
            && self.external_write_credentials_absent
    }
}

/// Admits a verifier-backed manifest against deployment-owned bindings.
pub fn admit(
    manifest: &VerifiedPromotionManifest,
    binding: &RuntimeBinding,
    now: i64,
    predecessor: Option<&AdmittedGate>,
) -> Result<AdmittedGate, PromotionError> {
    let manifest = manifest.manifest();
    manifest.validate_shape()?;
    if manifest.revoked
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
    match (manifest.gate, predecessor) {
        (Gate::A, None) => Ok(AdmittedGate {
            gate: Gate::A,
            digest: manifest.manifest_digest.clone(),
        }),
        (Gate::B, Some(previous))
            if previous.gate == Gate::A
                && manifest.predecessor_digest.as_deref() == Some(previous.digest.as_str()) =>
        {
            Ok(AdmittedGate {
                gate: Gate::B,
                digest: manifest.manifest_digest.clone(),
            })
        }
        (Gate::C, Some(previous))
            if previous.gate == Gate::A
                && manifest.predecessor_digest.as_deref() == Some(previous.digest.as_str()) =>
        {
            Ok(AdmittedGate {
                gate: Gate::C,
                digest: manifest.manifest_digest.clone(),
            })
        }
        _ => Err(PromotionError::InvalidPredecessor),
    }
}

/// Returns whether the service must remain in shadow mode.
pub fn shadow_only(gate_a: Result<AdmittedGate, PromotionError>) -> bool {
    gate_a.is_err()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::promotion::{Gate, PromotionManifest, test_verified};

    fn manifest(gate: Gate, predecessor: Option<String>) -> VerifiedPromotionManifest {
        let mut value = PromotionManifest {
            schema: "KTD21/v1".into(),
            gate,
            manifest_digest: String::new(),
            predecessor_digest: predecessor,
            corpus_digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            results_digest:
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            policy_version: "policy-1".into(),
            config_version: "config-1".into(),
            binary_digest:
                "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into(),
            issued_at: 10,
            expires_at: 20,
            key_id: "key-1".into(),
            generation: 1,
            revoked: false,
        };
        value.manifest_digest = value.derived_digest();
        test_verified(value)
    }

    fn binding() -> RuntimeBinding {
        RuntimeBinding {
            binary_digest:
                "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into(),
            policy_version: "policy-1".into(),
            config_version: "config-1".into(),
            corpus_digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            generation: 1,
            revoked_key_ids: vec![],
        }
    }

    #[test]
    fn gitops_gate_depends_on_shadow_gate_not_runtime_mutation_authority() {
        // Given a verifier-backed Gate A followed by Gate B and Gate C.
        let gate_a = admit(&manifest(Gate::A, None), &binding(), 15, None).expect("Gate A");
        let gate_b = admit(
            &manifest(Gate::B, Some(gate_a.digest.clone())),
            &binding(),
            15,
            Some(&gate_a),
        )
        .expect("Gate B");
        let gate_c = admit(
            &manifest(Gate::C, Some(gate_a.digest.clone())),
            &binding(),
            15,
            Some(&gate_a),
        )
        .expect("Gate C");
        // When Gate C instead names runtime-mutation Gate B as its predecessor.
        let rejected = admit(
            &manifest(Gate::C, Some(gate_b.digest.clone())),
            &binding(),
            15,
            Some(&gate_b),
        );
        // Then GitOps is independently gated by shadow qualification, never runtime authority.
        assert_eq!(gate_c.gate, Gate::C);
        assert_eq!(rejected, Err(PromotionError::InvalidPredecessor));
    }

    #[test]
    fn shadow_policy_is_loaded_and_requires_all_controls() {
        // Given the checked-in flat scalar Gate A policy.
        let policy =
            ShadowGatePolicy::from_yaml(include_str!("../../config/evaluation/shadow-gate.yaml"))
                .expect("policy");
        // When synthetic results meet its declared thresholds.
        // Then shadow remains the only admitted runtime mode.
        assert!(policy.admits_shadow(20, 5, 3));
        assert!(!policy.admits_shadow(19, 5, 3));
    }
}
