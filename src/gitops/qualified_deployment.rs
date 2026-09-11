//! Assesses bounded post-deployment health evidence without asserting its authenticity.
//!
//! Passing this pure check is necessary, not sufficient, for qualification. The
//! ingestion boundary must independently establish protected completion provenance,
//! deployed commit/image identity, and durable storage before producing a repair.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A currently eligible receipt loaded from the authoritative journal database.
/// It is not deserializable and grants no GitHub or runtime authority.
pub struct QualifiedDeployment {
    pub(crate) wire: super::receipt::ReceiptWire,
}

impl QualifiedDeployment {
    /// Immutable image from the protected, durably qualified receipt.
    pub fn image(&self) -> &str {
        &self.wire.image
    }
    /// Commit actually observed by the trusted deployment producer.
    pub fn commit(&self) -> &str {
        &self.wire.commit
    }
    /// Identity of the protected completion event.
    pub fn deployment_id(&self) -> &str {
        &self.wire.deployment_id
    }
    /// Content identity of qualification inputs, including health and policy.
    pub fn receipt_digest(&self) -> String {
        self.wire.digest()
    }
}

/// Deployment-owned thresholds declared before collecting the evidence window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationPolicy {
    /// Required duration after completion, in seconds (60 through 86400).
    pub window_seconds: u64,
    /// Maximum gap between observations, including the initial gap (1 through window duration).
    pub max_gap_seconds: u64,
    /// Maximum age of the completed observation window (1 through 604800 seconds).
    pub max_age_seconds: u64,
}

/// A paired observation for the exact same deployment identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthSample {
    /// Unix second at which both checks were observed; joining identities is owned by ingestion.
    pub observed_at: u64,
    /// Strict Gatus success, not missing data or an aggregate success fraction.
    pub gatus_healthy: bool,
    /// Strict Prometheus success, not missing data or a model interpretation.
    pub prometheus_healthy: bool,
}

/// Reasons to retain recommendation-only behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum QualificationError {
    /// Invalid limits or time arithmetic cannot define an evidence window.
    #[error("invalid observation policy or completion time")]
    InvalidPolicy,
    /// Evidence is missing, unhealthy, unordered, outside the window, or insufficiently dense.
    #[error("healthy post-deployment coverage is incomplete")]
    IncompleteHealth,
    /// The observation window has not completed or is no longer current.
    #[error("deployment health evidence is not current")]
    NotCurrent,
}

/// Checks chronological, paired success through the declared post-completion window.
///
/// This does not authenticate events, verify a deployed digest, or grant write authority.
/// Callers must not relabel untrusted model output as a qualified deployment.
pub fn assess_health(
    policy: &ObservationPolicy,
    completed_at: u64,
    now: u64,
    samples: &[HealthSample],
) -> Result<(), QualificationError> {
    if !(60..=86400).contains(&policy.window_seconds)
        || !(1..=policy.window_seconds).contains(&policy.max_gap_seconds)
        || !(1..=604800).contains(&policy.max_age_seconds)
    {
        return Err(QualificationError::InvalidPolicy);
    }
    let end = completed_at
        .checked_add(policy.window_seconds)
        .ok_or(QualificationError::InvalidPolicy)?;
    let last_allowed = end
        .checked_add(policy.max_gap_seconds)
        .ok_or(QualificationError::InvalidPolicy)?;
    if now < end || now - end > policy.max_age_seconds {
        return Err(QualificationError::NotCurrent);
    }
    if samples.is_empty() || samples.len() > 10000 {
        return Err(QualificationError::IncompleteHealth);
    }
    let mut previous = completed_at;
    for sample in samples {
        if !sample.gatus_healthy
            || !sample.prometheus_healthy
            || sample.observed_at <= previous
            || sample.observed_at > last_allowed
            || sample.observed_at > now
            || sample.observed_at - previous > policy.max_gap_seconds
        {
            return Err(QualificationError::IncompleteHealth);
        }
        previous = sample.observed_at;
    }
    // Require coverage through the end without requiring scrape clocks to align exactly.
    if previous < end {
        return Err(QualificationError::IncompleteHealth);
    }
    Ok(())
}
