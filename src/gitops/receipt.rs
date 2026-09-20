//! Reads bounded deployment receipts from a root-controlled, symlink-free Linux inbox.
//!
//! A trusted producer must join protected deployment completion, the observed
//! commit/image, and strict Gatus/Prometheus samples. This reader authenticates
//! filesystem provenance, not upstream APIs. Run AI-SRE as a non-root user; root
//! and the producer are trusted. No model-facing or network receipt import exists.

use super::{
    it_tools_image::valid_image,
    qualified_deployment::{HealthSample, ObservationPolicy},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Component, Path},
};
use thiserror::Error;

const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;

/// A receipt whose bytes came from protected local storage; health is not yet qualified.
/// This type cannot be deserialized or constructed from caller-supplied JSON.
pub struct ProtectedReceipt {
    pub(crate) wire: ReceiptWire,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReceiptWire {
    pub schema: String,
    pub repository: String,
    pub service: String,
    pub deployment_id: String,
    pub commit: String,
    pub image: String,
    pub observed_commit: String,
    pub observed_image: String,
    pub completed_at: u64,
    pub policy_declared_at: u64,
    pub policy: ObservationPolicy,
    pub samples: Vec<HealthSample>,
    pub deployment_succeeded: bool,
    pub revoked: bool,
}

/// Safe failures expose no receipt contents or filesystem details.
#[derive(Debug, Error)]
pub enum ReceiptError {
    /// Missing, mutable, linked, oversized, or unreadable inbox data is untrusted.
    #[error("deployment receipt is not protected")]
    Unprotected,
    /// The fixed schema or pilot identity is invalid.
    #[error("deployment receipt has invalid fields")]
    InvalidFields,
}

impl ProtectedReceipt {
    /// Reads an absolute file path under root-owned directories with no group/other writes.
    /// Root must publish regular files by atomic rename and must not modify opened files in place.
    pub fn read(path: &Path) -> Result<Self, ReceiptError> {
        // The production protection contract is Linux DAC without writable ACL masks.
        // Other platforms need their own ACL-aware boundary rather than assuming Unix mode bits suffice.
        if !cfg!(target_os = "linux") {
            return Err(ReceiptError::Unprotected);
        }
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return Err(ReceiptError::Unprotected);
        }
        for ancestor in path.ancestors() {
            let metadata = fs::symlink_metadata(ancestor).map_err(|_| ReceiptError::Unprotected)?;
            if metadata.uid() != 0
                || metadata.mode() & 0o022 != 0
                || metadata.file_type().is_symlink()
                || (ancestor != path && !metadata.is_dir())
            {
                return Err(ReceiptError::Unprotected);
            }
        }
        let before = fs::symlink_metadata(path).map_err(|_| ReceiptError::Unprotected)?;
        if !before.is_file() || before.len() > MAX_RECEIPT_BYTES || before.nlink() != 1 {
            return Err(ReceiptError::Unprotected);
        }
        let file = File::open(path).map_err(|_| ReceiptError::Unprotected)?;
        let opened = file.metadata().map_err(|_| ReceiptError::Unprotected)?;
        if opened.dev() != before.dev()
            || opened.ino() != before.ino()
            || opened.uid() != 0
            || opened.mode() & 0o022 != 0
            || !opened.is_file()
        {
            return Err(ReceiptError::Unprotected);
        }
        let mut bytes = Vec::new();
        file.take(MAX_RECEIPT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ReceiptError::Unprotected)?;
        if bytes.len() as u64 > MAX_RECEIPT_BYTES {
            return Err(ReceiptError::Unprotected);
        }
        let wire: ReceiptWire =
            serde_json::from_slice(&bytes).map_err(|_| ReceiptError::InvalidFields)?;
        wire.validate_shape()?;
        Ok(Self { wire })
    }

    /// Stable producer event identity, used for replay and contradiction detection.
    pub fn deployment_id(&self) -> &str {
        &self.wire.deployment_id
    }
}

impl ReceiptWire {
    pub(crate) fn validate_shape(&self) -> Result<(), ReceiptError> {
        if self.schema != "ai-sre/deployment-receipt/v1"
            || self.repository != "bodhispace-xyz/bodhispace-homelab"
            || self.service != "utility/it-tools"
            || self.deployment_id.is_empty()
            || self.deployment_id.len() > 128
            || !self
                .deployment_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            || !super::validate::valid_sha(&self.commit)
            || !super::validate::valid_sha(&self.observed_commit)
            || !valid_image(&self.image)
            || !valid_image(&self.observed_image)
            || self.completed_at > i64::MAX as u64
            || self.samples.len() > 10000
        {
            return Err(ReceiptError::InvalidFields);
        }
        Ok(())
    }

    pub(crate) fn digest(&self) -> String {
        digest(&serde_json::to_vec(self).expect("fixed receipt schema is serializable"))
    }
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

#[cfg(test)]
pub(crate) fn fixture() -> ProtectedReceipt {
    let image = format!("ghcr.io/corentinth/it-tools@sha256:{}", "a".repeat(64));
    ProtectedReceipt {
        wire: ReceiptWire {
            schema: "ai-sre/deployment-receipt/v1".into(),
            repository: "bodhispace-xyz/bodhispace-homelab".into(),
            service: "utility/it-tools".into(),
            deployment_id: "deployment-123-1".into(),
            commit: "b".repeat(40),
            image: image.clone(),
            observed_commit: "b".repeat(40),
            observed_image: image,
            completed_at: 100,
            policy_declared_at: 90,
            policy: ObservationPolicy {
                window_seconds: 120,
                max_gap_seconds: 60,
                max_age_seconds: 600,
            },
            samples: [101, 161, 221]
                .into_iter()
                .map(|observed_at| HealthSample {
                    observed_at,
                    gatus_healthy: true,
                    prometheus_healthy: true,
                })
                .collect(),
            deployment_succeeded: true,
            revoked: false,
        },
    }
}
