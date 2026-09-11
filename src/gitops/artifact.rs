//! Produces bounded, non-executable manual repair candidates without claiming sandbox validation.
//!
//! Candidates contain only the changed image line and controlled metadata. They
//! are not publish-ready; repository provenance and offline sandbox validation
//! must be completed before operator handoff. No GitHub credential is consumed.

use super::{
    it_tools_image::{PILOT_PATH, render},
    qualified_deployment::QualifiedDeployment,
    receipt::digest,
    validate::validate_change,
};
use serde::{Deserialize, Serialize};

/// Deployment-owned context for preparing a candidate from a stored qualification.
/// Source and revisions must come from read-only repository evidence, never model output.
pub struct ManualRepairRequest<'a> {
    /// Protected completion identity to resolve in the journal database.
    pub deployment_id: &'a str,
    /// Server-owned opaque incident identity, used for a relative operator-console link.
    pub incident_id: &'a str,
    /// Server-owned run identity for journal scope.
    pub run_id: &'a str,
    /// Base revision captured with the evidence.
    pub expected_base: &'a str,
    /// Base revision observed again immediately before preparation.
    pub current_base: &'a str,
    /// Digest identifying the redacted incident evidence.
    pub evidence_digest: &'a str,
    /// Exact pilot file from the captured base.
    pub source: &'a str,
    /// Current Unix second, injected by the application clock.
    pub now: u64,
}

/// Immutable candidate data; absence of a validation receipt means it is not publish-ready.
pub struct ManualRepairCandidate {
    wire: CandidateWire,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateWire {
    schema: String,
    title: String,
    incident_path: String,
    evidence_citations: Vec<EvidenceCitation>,
    incident_digest: String,
    run_digest: String,
    base_sha: String,
    source_digest: String,
    evidence_digest: String,
    qualification_digest: String,
    image: String,
    patch: String,
    description: String,
}

/// Journal-derived references; raw queries and tool payloads never enter the handoff.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceCitation {
    pub(crate) evidence_id: String,
    pub(crate) source: crate::reasoning::evidence::EvidenceSource,
    pub(crate) content_digest: String,
}

impl ManualRepairCandidate {
    /// Identity of every byte of canonical candidate data, including source and qualification bindings.
    pub fn artifact_digest(&self) -> String {
        digest(self.json().as_bytes())
    }
    /// A one-line unified diff for the fixed pilot path; never an executable script.
    pub fn patch(&self) -> &str {
        &self.wire.patch
    }
    /// Controlled suggested PR text, explicitly stating that validation has not run.
    pub fn description(&self) -> &str {
        &self.wire.description
    }
    /// Canonical data for durable storage; this is a candidate, not a publication receipt.
    pub fn json(&self) -> String {
        serde_json::to_string(&self.wire).expect("fixed candidate schema is serializable")
    }

    /// Qualified replacement image used when capturing an immutable validation snapshot.
    pub fn image(&self) -> &str {
        &self.wire.image
    }

    pub(crate) fn qualification_digest(&self) -> &str {
        &self.wire.qualification_digest
    }

    pub(crate) fn evidence_digest(&self) -> &str {
        &self.wire.evidence_digest
    }

    pub(crate) fn binds_handoff(
        &self,
        scope: &crate::reasoning::journal::JournalContext,
        base: &str,
    ) -> bool {
        self.wire.base_sha == base
            && self.wire.incident_digest == digest(scope.incident_id.as_bytes())
            && self.wire.run_digest == digest(scope.run_id.as_bytes())
    }

    pub(crate) fn binds_snapshot(&self, snapshot: &super::snapshot::RepositorySnapshot) -> bool {
        self.wire.base_sha == snapshot.base_sha()
            && self.wire.source_digest == digest(snapshot.original_source().as_bytes())
            && render(snapshot.original_source(), self.image()).as_deref()
                == Ok(snapshot.pilot_source())
    }

    pub(crate) fn build(
        request: &ManualRepairRequest<'_>,
        qualified: &QualifiedDeployment,
        evidence_citations: Vec<EvidenceCitation>,
    ) -> Option<Self> {
        if !safe_id(request.incident_id)
            || !safe_id(request.run_id)
            || !valid_digest(request.evidence_digest)
        {
            return None;
        }
        let after = render(request.source, qualified.image()).ok()?;
        validate_change(
            request.expected_base,
            request.current_base,
            request.source,
            &after,
            qualified.image(),
            &[PILOT_PATH],
        )
        .ok()?;
        let mut changed = request
            .source
            .split_inclusive('\n')
            .zip(after.split_inclusive('\n'))
            .enumerate()
            .filter(|(_, (a, b))| a != b);
        let (index, (before_line, after_line)) = changed.next()?;
        if changed.next().is_some() || before_line.contains('#') {
            return None;
        }
        let scalar = before_line.trim().strip_prefix("image:")?.trim();
        let old_image: String = serde_yaml_ng::from_str(scalar).ok()?;
        if !old_image.starts_with("ghcr.io/corentinth/it-tools:")
            && !old_image.starts_with("ghcr.io/corentinth/it-tools@sha256:")
        {
            return None;
        }
        if old_image.len() > 256
            || !old_image
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"./:@_-".contains(&b))
        {
            return None;
        }
        let line = index + 1;
        let mut patch =
            format!("--- a/{PILOT_PATH}\n+++ b/{PILOT_PATH}\n@@ -{line},1 +{line},1 @@\n");
        for (prefix, content) in [('-', before_line), ('+', after_line)] {
            patch.push(prefix);
            patch.push_str(content);
            if !content.ends_with('\n') {
                patch.push_str("\n\\ No newline at end of file\n");
            }
        }
        let qualification_digest = qualified.receipt_digest();
        let description = format!(
            "Manual repair candidate — not publish-ready.\n\nTarget: utility/it-tools; path: {PILOT_PATH}.\nBase: {}\nQualified deployment receipt: {qualification_digest}\nEvidence: {}\nExpected effect: restore the qualified immutable image.\nRollback: restore the previous image scalar only after fresh evidence and review.\nLocal scope: passed. Sandbox validation: not run. Remote checks: not run.\nOperator application: after fresh validation and review, use a clean checkout at the exact base above. This secret-safe zero-context patch requires git apply --check --index --unidiff-zero before git apply --index --unidiff-zero. Stop on a changed base or failed check; never force or fuzz the patch. See packaging/validator/OPERATOR.md in ai-sre for the guarded procedure.\nNo PR was created; merge and deployment remain operator-owned.\n",
            request.expected_base, request.evidence_digest
        );
        Some(Self {
            wire: CandidateWire {
                schema: "ai-sre/manual-repair-candidate/v1".into(),
                title: "fix(utility): restore qualified it-tools image".into(),
                incident_path: format!("/incidents/{}", request.incident_id),
                evidence_citations,
                incident_digest: digest(request.incident_id.as_bytes()),
                run_digest: digest(request.run_id.as_bytes()),
                base_sha: request.expected_base.into(),
                source_digest: digest(request.source.as_bytes()),
                evidence_digest: request.evidence_digest.into(),
                qualification_digest,
                image: qualified.image().into(),
                patch,
                description,
            },
        })
    }
}

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
