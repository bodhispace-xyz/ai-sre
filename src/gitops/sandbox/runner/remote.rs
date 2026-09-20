//! Carries bounded typed validation jobs over a deployment-owned SSH channel.
//!
//! SSH authenticates peers; the fixed worker independently enforces image and resource enrollment.
//! Neither request data nor response data is a command, publication approval, or deployment request.

use super::super::{SandboxLimits, SandboxPlan};
use super::*;
use crate::gitops::{artifact::ManualRepairCandidate, snapshot::RepositorySnapshot};
use tokio::io::{AsyncWrite, AsyncWriteExt};

const WIRE_LIMIT: usize = 16 * 1024;

/// A typed, expiring request bound to a locally prepared candidate and snapshot.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteJob {
    schema: String,
    nonce: String,
    artifact: String,
    snapshot: String,
    base: String,
    replacement: String,
    validator_image: String,
    runtime_digest: String,
    limits: SandboxLimits,
    issued_at: u64,
    expires_at: u64,
}

impl RemoteJob {
    /// Creates a fresh request only after local candidate/snapshot binding succeeds.
    pub fn new(
        candidate: &ManualRepairCandidate,
        snapshot: &RepositorySnapshot,
        policy: &crate::gitops::handoff::HandoffPolicy,
        now: u64,
    ) -> Result<Self, SandboxError> {
        if !candidate.binds_snapshot(snapshot) {
            return Err(SandboxError::InvalidConfiguration);
        }
        SandboxPlan::new(&policy.validator_image, policy.sandbox_limits.clone())?;
        let mut entropy = [0u8; 32];
        fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut entropy))
            .map_err(|_| SandboxError::Unavailable)?;
        Ok(Self {
            schema: "ai-sre/worker-request/v1".into(),
            nonce: digest(&entropy),
            artifact: candidate.artifact_digest(),
            snapshot: snapshot.digest().into(),
            base: snapshot.base_sha().into(),
            replacement: candidate.image().into(),
            validator_image: policy.validator_image.clone(),
            runtime_digest: policy.runtime_digest.clone(),
            limits: policy.sandbox_limits.clone(),
            issued_at: now,
            expires_at: now
                .checked_add(policy.sandbox_limits.timeout_seconds + 60)
                .ok_or(SandboxError::InvalidConfiguration)?,
        })
    }

    fn check(
        &self,
        policy: &crate::gitops::handoff::HandoffPolicy,
        now: u64,
    ) -> Result<(), SandboxError> {
        SandboxPlan::new(&policy.validator_image, policy.sandbox_limits.clone())?;
        if !(1..=3600).contains(&policy.max_validation_age_seconds) {
            return Err(SandboxError::InvalidConfiguration);
        }
        let valid_digest = |s: &str| {
            s.strip_prefix("sha256:").is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        };
        if self.schema != "ai-sre/worker-request/v1"
            || !valid_digest(&self.nonce)
            || !valid_digest(&self.artifact)
            || !valid_digest(&self.snapshot)
            || self.validator_image != policy.validator_image
            || self.runtime_digest != policy.runtime_digest
            || serde_json::to_vec(&self.limits).ok()
                != serde_json::to_vec(&policy.sandbox_limits).ok()
            || now < self.issued_at
            || now >= self.expires_at
            || self.expires_at.checked_sub(self.issued_at) != Some(self.limits.timeout_seconds + 60)
        {
            return Err(SandboxError::InvalidConfiguration);
        }
        SandboxPlan::new(&policy.validator_image, policy.sandbox_limits.clone())?;
        Ok(())
    }
}

/// Independently provisioned worker paths and validation enrollment, never request-selected.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfig {
    /// Read-only, pre-provisioned Git mirror with the requested HEAD; no fetch is performed.
    pub repository: PathBuf,
    /// Protected digest-pinned Podman executable.
    pub runtime: PathBuf,
    /// Private durable worker directory, shared by all forced-command invocations.
    pub state: PathBuf,
    /// Deployment-owned substantive image, runtime digest, and resource limits.
    pub policy: crate::gitops::handoff::HandoffPolicy,
}

fn now() -> Result<u64, SandboxError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| SandboxError::Unavailable)
}

/// Serves one length-framed request on an already SSH-authenticated forced-command stream.
/// The caller must await completion; channel failure never creates a success receipt.
pub async fn serve_worker<R: tokio::io::AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    config: &WorkerConfig,
    input: &mut R,
    output: &mut W,
) -> Result<(), SandboxError> {
    let bytes = tokio::time::timeout(Duration::from_secs(10), read_frame(input))
        .await
        .map_err(|_| SandboxError::Failed)??;
    let job: RemoteJob =
        serde_json::from_slice(&bytes).map_err(|_| SandboxError::InvalidConfiguration)?;
    job.check(&config.policy, now()?)?;
    let mut owner = RootlessValidator::connect_with_state(
        &config.runtime,
        &config.policy.runtime_digest,
        &config.state,
    )
    .await?;
    owner
        .jobs
        .as_mut()
        .ok_or(SandboxError::Failed)?
        .claim_request(&job.nonce)?;
    let remaining = Duration::from_secs(job.expires_at.saturating_sub(now()?));
    let snapshot = tokio::time::timeout(
        remaining,
        RepositorySnapshot::capture(&config.repository, &job.base, &job.replacement),
    )
    .await
    .map_err(|_| SandboxError::Failed)?
    .map_err(|_| SandboxError::Failed)?;
    if snapshot.digest() != job.snapshot {
        return Err(SandboxError::Failed);
    }
    job.check(&config.policy, now()?)?;
    let plan = SandboxPlan::new(
        &config.policy.validator_image,
        config.policy.sandbox_limits.clone(),
    )?;
    let arguments = plan.arguments(
        "u9-remote",
        snapshot
            .source_directory()
            .to_str()
            .ok_or(SandboxError::Failed)?,
    )?;
    let prepared = PreparedSandbox {
        snapshot,
        artifact_digest: job.artifact.clone(),
        arguments,
        limits: config.policy.sandbox_limits.clone(),
        image: config.policy.validator_image.clone(),
    };
    let receipt = owner
        .validate_until_shutdown(
            prepared,
            tokio::time::sleep(Duration::from_secs(job.expires_at.saturating_sub(now()?))),
        )
        .await?;
    job.check(&config.policy, now()?)?;
    let response = serde_json::to_vec(&serde_json::json!({"schema":"ai-sre/worker-response/v1", "nonce": job.nonce, "receipt": receipt})).map_err(|_| SandboxError::Failed)?;
    tokio::time::timeout(Duration::from_secs(10), write_frame(output, &response))
        .await
        .map_err(|_| SandboxError::Failed)?
}

async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    input: &mut R,
) -> Result<Vec<u8>, SandboxError> {
    let length = input.read_u32().await.map_err(|_| SandboxError::Failed)? as usize;
    if length == 0 || length > WIRE_LIMIT {
        return Err(SandboxError::Failed);
    }
    let mut bytes = vec![0; length];
    input
        .read_exact(&mut bytes)
        .await
        .map_err(|_| SandboxError::Failed)?;
    Ok(bytes)
}

async fn write_frame<W: AsyncWrite + Unpin>(
    output: &mut W,
    bytes: &[u8],
) -> Result<(), SandboxError> {
    if bytes.is_empty() || bytes.len() > WIRE_LIMIT {
        return Err(SandboxError::Failed);
    }
    output
        .write_u32(bytes.len() as u32)
        .await
        .map_err(|_| SandboxError::Failed)?;
    output
        .write_all(bytes)
        .await
        .map_err(|_| SandboxError::Failed)?;
    output.flush().await.map_err(|_| SandboxError::Failed)
}

/// Deployment-owned SSH identity and host enrollment; never populated by model output.
pub struct SshWorkerConfig {
    /// Dedicated worker DNS name or IPv4 address.
    pub host: String,
    /// Dedicated forced-command account.
    pub user: String,
    /// Explicit SSH port.
    pub port: u16,
    /// Dedicated service key, not an operator key or SSH agent.
    pub identity_file: PathBuf,
    /// Independently enrolled host key file; unknown keys are rejected.
    pub known_hosts: PathBuf,
}

/// Serialized authenticated client; no retry occurs after an uncertain remote launch.
pub struct SshValidator {
    config: SshWorkerConfig,
}

fn trusted_ssh_file(path: &Path, private: bool) -> Result<(), SandboxError> {
    let uid = rustix::process::geteuid().as_raw();
    if !path.is_absolute() {
        return Err(SandboxError::Unavailable);
    }
    for (index, ancestor) in path.ancestors().enumerate() {
        let metadata = fs::symlink_metadata(ancestor).map_err(|_| SandboxError::Unavailable)?;
        if metadata.uid() != 0 && metadata.uid() != uid {
            return Err(SandboxError::Unavailable);
        }
        if index == 0 {
            if !metadata.is_file()
                || metadata.nlink() != 1
                || metadata.mode() & 0o022 != 0
                || (private && metadata.mode() & 0o077 != 0)
            {
                return Err(SandboxError::Unavailable);
            }
        } else {
            // Root-owned sticky directories (such as /tmp) protect their owned child entries.
            let protected_sticky = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
            if !metadata.is_dir() || (metadata.mode() & 0o022 != 0 && !protected_sticky) {
                return Err(SandboxError::Unavailable);
            }
        }
    }
    Ok(())
}

impl SshValidator {
    /// Validates fixed deployment routing and credential paths without opening a connection.
    pub fn new(config: SshWorkerConfig) -> Result<Self, SandboxError> {
        let safe = |s: &str| {
            !s.is_empty()
                && s.len() <= 253
                && s.as_bytes()[0].is_ascii_alphanumeric()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
        };
        if !safe(&config.host) || !safe(&config.user) || config.port == 0 {
            return Err(SandboxError::InvalidConfiguration);
        }
        for path in [&config.identity_file, &config.known_hosts] {
            if !path.is_absolute()
                || path
                    .to_str()
                    .is_none_or(|s| s.bytes().any(|b| b <= 32 || b"%$~\"\\".contains(&b)))
            {
                return Err(SandboxError::InvalidConfiguration);
            }
        }
        Ok(Self { config })
    }

    /// Imports only an authenticated, fresh, exactly bound response after the SSH process succeeds.
    /// On timeout the local SSH child is killed and joined; the remote job remains deadline-bounded.
    pub async fn validate(
        &mut self,
        job: &RemoteJob,
        policy: &crate::gitops::handoff::HandoffPolicy,
    ) -> Result<ValidationReceipt, SandboxError> {
        job.check(policy, now()?)?;
        trusted_ssh_file(&self.config.identity_file, true)?;
        trusted_ssh_file(&self.config.known_hosts, false)?;
        let mut command = Command::new("/usr/bin/ssh");
        command
            .env_clear()
            .current_dir("/")
            .args(["-F", "/dev/null", "-T", "-a", "-x"]);
        for option in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "IdentitiesOnly=yes",
            "IdentityAgent=none",
            "ForwardAgent=no",
            "ClearAllForwardings=yes",
            "ControlMaster=no",
            "ControlPath=none",
            "ProxyCommand=none",
            "ProxyJump=none",
            "PermitLocalCommand=no",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "PreferredAuthentications=publickey",
            "GlobalKnownHostsFile=/dev/null",
            "UpdateHostKeys=no",
            "ConnectTimeout=10",
            "ConnectionAttempts=1",
            "ServerAliveInterval=5",
            "ServerAliveCountMax=2",
        ] {
            command.args(["-o", option]);
        }
        command
            .arg("-o")
            .arg(format!(
                "UserKnownHostsFile={}",
                self.config.known_hosts.display()
            ))
            .arg("-i")
            .arg(&self.config.identity_file)
            .arg("-p")
            .arg(self.config.port.to_string())
            .arg("-l")
            .arg(&self.config.user)
            .arg(&self.config.host)
            .arg("ai-sre-validator-v1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let bytes = serde_json::to_vec(job).map_err(|_| SandboxError::Failed)?;
        let mut child = command.spawn().map_err(|_| SandboxError::Unavailable)?;
        let deadline = Duration::from_secs(job.expires_at.saturating_sub(now()?));
        let result = tokio::time::timeout(deadline, async {
            let mut stdin = child.stdin.take().ok_or(SandboxError::Failed)?;
            write_frame(&mut stdin, &bytes).await?;
            drop(stdin);
            let mut stdout = child.stdout.take().ok_or(SandboxError::Failed)?;
            let response = read_frame(&mut stdout).await?;
            let mut trailing = [0u8; 1];
            if stdout
                .read(&mut trailing)
                .await
                .map_err(|_| SandboxError::Failed)?
                != 0
                || !child
                    .wait()
                    .await
                    .map_err(|_| SandboxError::Failed)?
                    .success()
            {
                return Err(SandboxError::Failed);
            }
            import_response(&response, job, policy, now()?)
        })
        .await
        .unwrap_or(Err(SandboxError::Failed));
        if result.is_err() {
            let _ = child.start_kill();
            tokio::time::timeout(Duration::from_secs(10), child.wait())
                .await
                .map_err(|_| SandboxError::Failed)?
                .map_err(|_| SandboxError::Failed)?;
        }
        result
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    schema: String,
    nonce: String,
    receipt: ReceiptWire,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptWire {
    schema: String,
    artifact_digest: String,
    snapshot_digest: String,
    validator_image: String,
    runtime_digest: String,
    policy_digest: String,
    output_digest: String,
    output_bytes: usize,
    completed_at: u64,
}

fn import_response(
    bytes: &[u8],
    job: &RemoteJob,
    policy: &crate::gitops::handoff::HandoffPolicy,
    now: u64,
) -> Result<ValidationReceipt, SandboxError> {
    job.check(policy, now)?;
    let response: Response = serde_json::from_slice(bytes).map_err(|_| SandboxError::Failed)?;
    let receipt = response.receipt;
    if response.schema != "ai-sre/worker-response/v1"
        || response.nonce != job.nonce
        || receipt.schema != "ai-sre/sandbox-validation/v1"
        || receipt.artifact_digest != job.artifact
        || receipt.snapshot_digest != job.snapshot
        || receipt.validator_image != policy.validator_image
        || receipt.runtime_digest != policy.runtime_digest
        || receipt.policy_digest
            != digest(
                &serde_json::to_vec(&policy.sandbox_limits).map_err(|_| SandboxError::Failed)?,
            )
        || receipt.output_bytes > policy.sandbox_limits.output_bytes
        || !receipt
            .output_digest
            .strip_prefix("sha256:")
            .is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        || receipt.completed_at < job.issued_at
        || receipt.completed_at > now
        || now.saturating_sub(receipt.completed_at) > policy.max_validation_age_seconds
    {
        return Err(SandboxError::Failed);
    }
    Ok(ValidationReceipt {
        schema: "ai-sre/sandbox-validation/v1",
        artifact_digest: receipt.artifact_digest,
        snapshot_digest: receipt.snapshot_digest,
        validator_image: receipt.validator_image,
        runtime_digest: receipt.runtime_digest,
        policy_digest: receipt.policy_digest,
        output_digest: receipt.output_digest,
        output_bytes: receipt.output_bytes,
        completed_at: receipt.completed_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_paths_reject_symlinked_and_writable_parents() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        // Given private files below a directory owned by this test's OS identity.
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "u9-ssh-paths-{}-{}",
                std::process::id(),
                NEXT_JOB.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let directory = root.join("credentials");
        fs::create_dir(&directory).unwrap();
        let key = directory.join("key");
        fs::write(&key, b"not-a-real-key").unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(trusted_ssh_file(&key, true).is_ok());
        // When a parent is symlinked or writable by another user, leaf permissions are insufficient.
        symlink(&directory, root.join("alias")).unwrap();
        assert!(trusted_ssh_file(&root.join("alias/key"), true).is_err());
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(trusted_ssh_file(&key, true).is_err());
        // Then restoring a protected parent admits the original file again.
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(trusted_ssh_file(&key, true).is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    fn fixture() -> (
        RemoteJob,
        crate::gitops::handoff::HandoffPolicy,
        serde_json::Value,
    ) {
        let policy = crate::gitops::handoff::HandoffPolicy {
            max_validation_age_seconds: 300,
            validator_image: format!(
                "ghcr.io/bodhispace-xyz/ai-sre-validator@sha256:{}",
                "a".repeat(64)
            ),
            runtime_digest: digest(b"runtime"),
            sandbox_limits: SandboxLimits::default(),
        };
        let job = RemoteJob {
            schema: "ai-sre/worker-request/v1".into(),
            nonce: digest(b"nonce"),
            artifact: digest(b"artifact"),
            snapshot: digest(b"snapshot"),
            base: "a".repeat(40),
            replacement: format!("ghcr.io/corentinth/it-tools@sha256:{}", "b".repeat(64)),
            validator_image: policy.validator_image.clone(),
            runtime_digest: policy.runtime_digest.clone(),
            limits: policy.sandbox_limits.clone(),
            issued_at: 100,
            expires_at: 280,
        };
        let response = serde_json::json!({"schema":"ai-sre/worker-response/v1", "nonce": job.nonce, "receipt": {"schema":"ai-sre/sandbox-validation/v1", "artifact_digest": job.artifact, "snapshot_digest": job.snapshot, "validator_image": policy.validator_image, "runtime_digest": policy.runtime_digest, "policy_digest": digest(&serde_json::to_vec(&policy.sandbox_limits).unwrap()), "output_digest": digest(b"output"), "output_bytes": 8, "completed_at": 110}});
        (job, policy, response)
    }

    #[test]
    fn authenticated_response_still_requires_exact_job_and_policy_binding() {
        // Given a correctly bound response received through the authenticated channel.
        let (job, policy, response) = fixture();
        assert!(
            import_response(&serde_json::to_vec(&response).unwrap(), &job, &policy, 120).is_ok()
        );
        // When any immutable receipt binding is substituted with another job's value.
        for field in [
            "artifact_digest",
            "snapshot_digest",
            "validator_image",
            "runtime_digest",
            "policy_digest",
            "schema",
            "output_digest",
        ] {
            let mut changed = response.clone();
            changed["receipt"][field] = "foreign".into();
            // Then authenticated transport alone cannot mint a matching validation receipt.
            assert!(
                import_response(&serde_json::to_vec(&changed).unwrap(), &job, &policy, 120)
                    .is_err(),
                "{field}"
            );
        }
        let mut changed = response.clone();
        changed["nonce"] = digest(b"another-request").into();
        assert!(
            import_response(&serde_json::to_vec(&changed).unwrap(), &job, &policy, 120).is_err()
        );
        assert!(
            import_response(&serde_json::to_vec(&response).unwrap(), &job, &policy, 280).is_err()
        );
    }

    #[tokio::test]
    async fn worker_rejects_expired_requests_before_opening_runtime_or_state() {
        // Given an expired framed request and deliberately nonexistent deployment paths.
        let (job, policy, _) = fixture();
        let config = WorkerConfig {
            repository: "/nonexistent/repository".into(),
            runtime: "/nonexistent/runtime".into(),
            state: "/nonexistent/state".into(),
            policy,
        };
        let mut wire = Vec::new();
        write_frame(&mut wire, &serde_json::to_vec(&job).unwrap())
            .await
            .unwrap();
        let mut output = Vec::new();
        // When the worker receives the request, policy rejection precedes any runtime admission.
        assert_eq!(
            serve_worker(&config, &mut wire.as_slice(), &mut output).await,
            Err(SandboxError::InvalidConfiguration)
        );
        // Then no success response is emitted.
        assert!(output.is_empty());
    }

    #[test]
    fn ssh_routing_cannot_inject_options_or_expand_credential_paths() {
        // Given model-like option syntax or SSH token expansion in deployment settings.
        for (host, path) in [
            ("-oProxyCommand=evil", "/keys/worker"),
            ("worker", "/keys/%h"),
            ("worker;evil", "/keys/worker"),
            ("worker", "/keys/with space"),
        ] {
            let config = SshWorkerConfig {
                host: host.into(),
                user: "validator".into(),
                port: 22,
                identity_file: path.into(),
                known_hosts: "/keys/known_hosts".into(),
            };
            // When constructing a client, these settings fail before any SSH process exists.
            assert!(SshValidator::new(config).is_err());
        }
    }
    #[tokio::test]
    async fn frame_rejects_oversized_lengths_before_reading_payload() {
        // Given an authenticated peer advertising more bytes than the protocol allows.
        let wire = ((WIRE_LIMIT + 1) as u32).to_be_bytes();
        let mut input = wire.as_slice();
        // When the frame is read, no payload allocation or execution is allowed.
        assert!(read_frame(&mut input).await.is_err());
    }
}
