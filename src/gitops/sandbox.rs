//! Fixed rootless container policy for offline repository validation, never model-authored commands.

use super::{artifact::ManualRepairCandidate, snapshot::RepositorySnapshot};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod jobs;
mod runner;
pub use runner::{RemoteJob, SshValidator, SshWorkerConfig, WorkerConfig, serve_worker};
pub use runner::{RootlessValidator, ValidationReceipt};

/// An owned validation input bound to one candidate. This is not a validation result.
/// Keep it alive while the container uses its source mount; dropping it removes the source.
pub struct PreparedSandbox {
    snapshot: RepositorySnapshot,
    artifact_digest: String,
    arguments: Vec<String>,
    limits: SandboxLimits,
    image: String,
}

impl PreparedSandbox {
    /// Fixed container arguments, including the owned read-only source mount.
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }
    /// Identity of the exact candidate authorized for this validation attempt.
    pub fn artifact_digest(&self) -> &str {
        &self.artifact_digest
    }
    /// Identity of the committed tree and deterministic replacement.
    pub fn snapshot_digest(&self) -> &str {
        self.snapshot.digest()
    }
}

/// Hard ceilings for the untrusted `make ci` workload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxLimits {
    /// Container deadline in seconds, from 1 through 600.
    pub timeout_seconds: u64,
    /// Memory ceiling in MiB, from 128 through 2048.
    pub memory_mib: u32,
    /// Maximum processes, from 16 through 512.
    pub pids: u32,
    /// CPU ceiling, from 1 through 4.
    pub cpus: u32,
    /// Total scratch ceiling in MiB (64 through 1024), including 16 MiB each for tmp and shm.
    pub scratch_mib: u32,
    /// Combined stdout/stderr bound in bytes, from 1024 through 1048576.
    pub output_bytes: usize,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            timeout_seconds: 120,
            memory_mib: 512,
            pids: 128,
            cpus: 1,
            scratch_mib: 256,
            output_bytes: 65536,
        }
    }
}

/// A deployment-owned pinned validator image and reviewed resource policy.
pub struct SandboxPlan {
    image: String,
    limits: SandboxLimits,
}

/// Failure to establish validation never yields an approval or a publish-ready artifact.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SandboxError {
    /// This candidate already has a durable attempt; explicit recovery is required before revalidation.
    #[error("validation attempt already exists; explicit recovery is required")]
    AttemptExists,
    /// Invalid image, path, name, or resource ceiling.
    #[error("invalid sandbox configuration")]
    InvalidConfiguration,
    /// A pinned local Linux rootless runtime could not be established.
    #[error("rootless validator is unavailable")]
    Unavailable,
    /// Validation exited unsuccessfully or exceeded its bounds.
    #[error("sandbox validation failed or exceeded its limits")]
    Failed,
}

impl SandboxPlan {
    /// Binds a candidate to an owned snapshot before constructing a container invocation.
    /// Does not start a process, establish runtime isolation, or grant handoff readiness.
    pub fn prepare(
        &self,
        name: &str,
        candidate: &ManualRepairCandidate,
        snapshot: RepositorySnapshot,
    ) -> Result<PreparedSandbox, SandboxError> {
        if !candidate.binds_snapshot(&snapshot) {
            return Err(SandboxError::InvalidConfiguration);
        }
        let source = snapshot.source_directory();
        let arguments = self.arguments(
            name,
            source.to_str().ok_or(SandboxError::InvalidConfiguration)?,
        )?;
        Ok(PreparedSandbox {
            snapshot,
            artifact_digest: candidate.artifact_digest(),
            arguments,
            limits: self.limits.clone(),
            image: self.image.clone(),
        })
    }

    /// Accepts only a digest-pinned image in the dedicated validator repository.
    pub fn new(image: &str, limits: SandboxLimits) -> Result<Self, SandboxError> {
        let pinned = image
            .strip_prefix("ghcr.io/bodhispace-xyz/ai-sre-validator@sha256:")
            .is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            });
        if !pinned
            || !(1..=600).contains(&limits.timeout_seconds)
            || !(128..=2048).contains(&limits.memory_mib)
            || !(16..=512).contains(&limits.pids)
            || !(1..=4).contains(&limits.cpus)
            || !(64..=1024).contains(&limits.scratch_mib)
            || !(1024..=1048576).contains(&limits.output_bytes)
        {
            return Err(SandboxError::InvalidConfiguration);
        }
        Ok(Self {
            image: image.into(),
            limits,
        })
    }

    /// Builds fixed argv for a private prepared snapshot; this function does not execute it.
    pub fn arguments(&self, name: &str, source: &str) -> Result<Vec<String>, SandboxError> {
        if !name.starts_with("u9-")
            || name.len() > 96
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || !source.starts_with('/')
            || source.len() > 1024
            || source.split('/').any(|p| p == ".." || p == ".")
            || source.bytes().any(|b| b < 32 || b == b',' || b == b'\\')
        {
            return Err(SandboxError::InvalidConfiguration);
        }
        Ok(vec![
            "--remote=false".into(),
            "run".into(),
            "--rm".into(),
            format!("--name={name}"),
            "--pull=never".into(),
            "--image-volume=ignore".into(),
            "--network=none".into(),
            "--read-only".into(),
            "--read-only-tmpfs=false".into(),
            "--cap-drop=ALL".into(),
            "--security-opt=no-new-privileges".into(),
            "--user=65532:65532".into(),
            "--pid=private".into(),
            "--ipc=private".into(),
            "--log-driver=none".into(),
            format!("--timeout={}", self.limits.timeout_seconds),
            format!("--memory={}m", self.limits.memory_mib),
            format!("--memory-swap={}m", self.limits.memory_mib),
            format!("--cpus={}", self.limits.cpus),
            format!("--pids-limit={}", self.limits.pids),
            "--shm-size=16m".into(),
            "--tmpfs".into(),
            format!(
                // Repository scripts and mirrored provider binaries execute only inside the sandbox.
                "/work:rw,nosuid,nodev,exec,size={}m,mode=1777",
                self.limits.scratch_mib - 32
            ),
            "--tmpfs".into(),
            "/tmp:rw,nosuid,nodev,noexec,size=16m,mode=1777".into(),
            "--tmpfs".into(),
            "/dev/shm:rw,nosuid,nodev,noexec,size=16m,mode=1777".into(),
            "--mount".into(),
            format!("type=bind,src={source},dst=/source,ro=true"),
            "--workdir=/work".into(),
            "--entrypoint=/usr/local/bin/validate-u9".into(),
            self.image.clone(),
        ])
    }
}
