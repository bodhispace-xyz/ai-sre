//! Runs bounded local rootless validation and retains cancelled jobs until explicit cleanup.
//!
//! The application must own one validator and call `cleanup` during shutdown.
//! Runtime helpers, the host, and the pinned validator image are deployment-trusted.
//! Dropping this owner is not an asynchronous cleanup guarantee: the container
//! deadline bounds a surviving workload, but crash recovery must remove stale jobs.
//! No caller-provided success flag can construct a validation receipt.

use super::jobs::JobStore;
use super::{PreparedSandbox, SandboxError};
use crate::gitops::receipt::digest;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
};

static NEXT_JOB: AtomicU64 = AtomicU64::new(0);
const CONTROL_LIMIT: usize = 64 * 1024;

mod remote;
pub use remote::{RemoteJob, SshValidator, SshWorkerConfig, WorkerConfig, serve_worker};

/// Evidence of one successful bounded validator run, not permission to publish or deploy.
#[derive(Serialize)]
pub struct ValidationReceipt {
    schema: &'static str,
    artifact_digest: String,
    snapshot_digest: String,
    validator_image: String,
    runtime_digest: String,
    policy_digest: String,
    output_digest: String,
    output_bytes: usize,
    completed_at: u64,
}

impl ValidationReceipt {
    /// Candidate identity checked before execution.
    pub fn artifact_digest(&self) -> &str {
        &self.artifact_digest
    }
    /// Identity of the exact captured source and repair.
    pub fn snapshot_digest(&self) -> &str {
        &self.snapshot_digest
    }
    /// Completion time from the application host's wall clock.
    pub fn completed_at(&self) -> u64 {
        self.completed_at
    }

    pub(crate) fn matches_enrollment(
        &self,
        image: &str,
        runtime_digest: &str,
        limits: &super::SandboxLimits,
    ) -> bool {
        self.validator_image == image
            && self.runtime_digest == runtime_digest
            && serde_json::to_vec(limits).is_ok_and(|limits| self.policy_digest == digest(&limits))
    }
}

/// A serialized local runtime owner. Cancellation retains its job until cleanup succeeds.
pub struct RootlessValidator {
    binary: PathBuf,
    binary_digest: String,
    home: OsString,
    runtime_directory: OsString,
    active: Option<Active>,
    jobs: Option<JobStore>,
    // Acceptance tests inspect bounded diagnostics without exporting workload content in production.
    #[cfg(test)]
    last_workload_output: Option<Output>,
}

struct Active {
    name: String,
    prepared: PreparedSandbox,
    child: Option<Child>,
}

impl RootlessValidator {
    /// Admits a root-owned digest-pinned Linux runtime with rootless cgroup-v2 enforcement.
    /// Does not install Podman, pull images, or fall back to a remote/rootful engine.
    pub async fn connect(binary: &Path, expected_digest: &str) -> Result<Self, SandboxError> {
        if !cfg!(target_os = "linux") || !binary.is_absolute() {
            return Err(SandboxError::Unavailable);
        }
        let binary = fs::canonicalize(binary).map_err(|_| SandboxError::Unavailable)?;
        for ancestor in binary.ancestors() {
            let metadata = fs::symlink_metadata(ancestor).map_err(|_| SandboxError::Unavailable)?;
            if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                return Err(SandboxError::Unavailable);
            }
        }
        let home = std::env::var_os("HOME").ok_or(SandboxError::Unavailable)?;
        let runtime_directory =
            std::env::var_os("XDG_RUNTIME_DIR").ok_or(SandboxError::Unavailable)?;
        if !Path::new(&home).is_absolute() || !Path::new(&runtime_directory).is_absolute() {
            return Err(SandboxError::Unavailable);
        }
        let mut validator = Self {
            binary,
            binary_digest: expected_digest.into(),
            home,
            runtime_directory,
            active: None,
            jobs: None,
            #[cfg(test)]
            last_workload_output: None,
        };
        validator.verify_binary()?;
        validator.check_mount_defaults()?;
        validator.probe().await?;
        Ok(validator)
    }

    /// Opens deployment-owned private state and reconciles any interrupted launch before admission.
    /// Use one state directory per dedicated worker runtime. It must already exist with mode 0700.
    /// Recovery removes only a recorded container and its unchanged, identity-checked snapshot.
    /// Interrupted work never becomes a validation result.
    pub async fn connect_with_state(
        binary: &Path,
        expected_digest: &str,
        state: &Path,
    ) -> Result<Self, SandboxError> {
        let mut validator = Self::connect(binary, expected_digest).await?;
        validator.jobs = Some(JobStore::open(state)?);
        validator.recover_pending().await?;
        Ok(validator)
    }

    async fn recover_pending(&mut self) -> Result<(), SandboxError> {
        if self.jobs.is_some() {
            self.verify_binary()?;
            let identity = self.runtime_identity().await?;
            self.jobs
                .as_mut()
                .ok_or(SandboxError::Failed)?
                .bind_runtime(&identity)?;
        }
        let Some(job) = self
            .jobs
            .as_ref()
            .map(JobStore::pending)
            .transpose()?
            .flatten()
        else {
            return Ok(());
        };
        self.verify_binary()?;
        let target = job.container_id.as_deref().unwrap_or(&job.name);
        let exists = self.control(&["container", "exists", target]).await?;
        match exists.code {
            // Absence alone cannot rule out an interrupted launcher creating the job later.
            Some(1) if job.container_id.is_some() => {}
            Some(0) => {
                let inspected = self
                    .control(&[
                        "inspect",
                        "--type=container",
                        "--format",
                        "{{json .}}",
                        target,
                    ])
                    .await?;
                if !inspected.success {
                    return Err(SandboxError::Failed);
                }
                let id = owned_container_id(&inspected.stdout, &job.name, &job.token)
                    .ok_or(SandboxError::Failed)?;
                if job.container_id.as_ref().is_some_and(|known| known != &id) {
                    return Err(SandboxError::Failed);
                }
                // Persist identity before deletion so a crash after removal remains recoverable.
                self.jobs
                    .as_mut()
                    .ok_or(SandboxError::Failed)?
                    .remember_container(&job, &id)?;
                // Immutable ID prevents a same-name replacement between inspection and removal.
                let removed = self
                    .control(&["rm", "--force", "--ignore", "--time=0", &id])
                    .await?;
                if !removed.success {
                    return Err(SandboxError::Failed);
                }
            }
            _ => return Err(SandboxError::Failed),
        }
        job.remove_snapshot()?;
        self.jobs.as_mut().ok_or(SandboxError::Failed)?.clear(&job)
    }

    fn verify_binary(&self) -> Result<(), SandboxError> {
        let file = fs::File::open(&self.binary).map_err(|_| SandboxError::Unavailable)?;
        let metadata = file.metadata().map_err(|_| SandboxError::Unavailable)?;
        if !metadata.is_file() || metadata.len() > 128 * 1024 * 1024 {
            return Err(SandboxError::Unavailable);
        }
        let mut bytes = Vec::new();
        file.take(128 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| SandboxError::Unavailable)?;
        if bytes.len() > 128 * 1024 * 1024 || digest(&bytes) != self.binary_digest {
            return Err(SandboxError::Unavailable);
        }
        Ok(())
    }

    async fn runtime_identity(&mut self) -> Result<String, SandboxError> {
        let output = self
            .control(&["info", "--format", "{{json .Store}}"])
            .await?;
        if !output.success {
            return Err(SandboxError::Unavailable);
        }
        let storage: RuntimeStorage =
            serde_json::from_slice(&output.stdout).map_err(|_| SandboxError::Unavailable)?;
        if storage.transient_store || storage.graph_driver_name.is_empty() {
            return Err(SandboxError::Unavailable);
        }
        let mut paths = Vec::new();
        for path in [
            &storage.graph_root,
            &storage.run_root,
            Path::new(&self.home),
            Path::new(&self.runtime_directory),
        ] {
            if !path.is_absolute() {
                return Err(SandboxError::Unavailable);
            }
            let path = fs::canonicalize(path).map_err(|_| SandboxError::Unavailable)?;
            if !path.is_dir() {
                return Err(SandboxError::Unavailable);
            }
            paths.push(path);
        }
        let root = fs::metadata(&paths[0]).map_err(|_| SandboxError::Unavailable)?;
        // Runroot can be recreated on reboot; only the persistent graphroot binds device/inode.
        let identity = (
            paths,
            root.dev(),
            root.ino(),
            root.uid(),
            storage.graph_driver_name,
        );
        Ok(digest(
            &serde_json::to_vec(&identity).map_err(|_| SandboxError::Unavailable)?,
        ))
    }

    fn check_mount_defaults(&self) -> Result<(), SandboxError> {
        // mounts.conf is independent of containers.conf and may expose host secrets.
        for path in [
            PathBuf::from("/usr/share/containers/mounts.conf"),
            PathBuf::from("/etc/containers/mounts.conf"),
            Path::new(&self.home).join(".config/containers/mounts.conf"),
        ] {
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(SandboxError::Unavailable),
            };
            if !metadata.is_file() || metadata.len() > CONTROL_LIMIT as u64 {
                return Err(SandboxError::Unavailable);
            }
            let mut text = String::new();
            fs::File::open(path)
                .map_err(|_| SandboxError::Unavailable)?
                .take(CONTROL_LIMIT as u64 + 1)
                .read_to_string(&mut text)
                .map_err(|_| SandboxError::Unavailable)?;
            if text.len() > CONTROL_LIMIT
                || text
                    .lines()
                    .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
            {
                return Err(SandboxError::Unavailable);
            }
        }
        Ok(())
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .arg("--remote=false")
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_RUNTIME_DIR", &self.runtime_directory)
            .env("PATH", "/usr/bin:/bin")
            .env("CONTAINERS_CONF", "/dev/null")
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    async fn control(&mut self, args: &[&str]) -> Result<Output, SandboxError> {
        let mut child = self
            .command()
            .args(args)
            .spawn()
            .map_err(|_| SandboxError::Unavailable)?;
        collect(&mut child, CONTROL_LIMIT, Duration::from_secs(10)).await
    }

    async fn probe(&mut self) -> Result<(), SandboxError> {
        let output = self
            .control(&["info", "--format", "{{json .Host}}"])
            .await?;
        if !output.success || !valid_host(&output.stdout) {
            return Err(SandboxError::Unavailable);
        }
        Ok(())
    }

    /// Runs the prepared workload and imports only bounded output identities.
    /// A cancelled future keeps its job in this owner; the next call cleans it first.
    pub async fn validate(
        &mut self,
        prepared: PreparedSandbox,
    ) -> Result<ValidationReceipt, SandboxError> {
        #[cfg(test)]
        {
            self.last_workload_output = None;
        }
        self.cleanup().await?;
        self.verify_binary()?;
        self.check_mount_defaults()?;
        self.probe().await?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| SandboxError::Unavailable)?
            .as_nanos();
        let mut name = format!(
            "u9-{}-{timestamp}-{}",
            std::process::id(),
            NEXT_JOB.fetch_add(1, Ordering::Relaxed)
        );
        let durable = self
            .jobs
            .as_mut()
            .map(|jobs| {
                jobs.begin(
                    &prepared.artifact_digest,
                    prepared.snapshot_digest(),
                    &prepared.image,
                    prepared
                        .snapshot
                        .source_directory()
                        .parent()
                        .ok_or(SandboxError::InvalidConfiguration)?,
                )
            })
            .transpose()?;
        if let Some(job) = &durable {
            name.clone_from(&job.name);
        }
        // Names belong to this runtime owner, never a model or a reusable caller-selected name.
        let mut args: Vec<_> = prepared
            .arguments
            .iter()
            .filter(|arg| arg.as_str() != "--remote=false" && arg.as_str() != "--rm")
            .map(|arg| {
                if arg.starts_with("--name=") {
                    format!("--name={name}")
                } else {
                    arg.clone()
                }
            })
            .collect();
        if let Some(job) = &durable {
            args.insert(
                args.len() - 1,
                format!("--label=io.bodhispace.ai-sre.job={}", job.token),
            );
        }
        let child = match self.command().args(args).spawn() {
            Ok(child) => child,
            Err(_) => {
                // Unlike a crash or a started Podman process, this OS error proves no launch ran.
                // No await separates the failed spawn from reconciling its durable intent.
                if let Some(job) = durable {
                    job.remove_snapshot()?;
                    self.jobs
                        .as_mut()
                        .ok_or(SandboxError::Failed)?
                        .clear(&job)?;
                }
                return Err(SandboxError::Unavailable);
            }
        };
        self.active = Some(Active {
            name,
            prepared,
            child: Some(child),
        });
        let result = self.finish().await;
        // No successful receipt escapes if removal fails. Ownership remains for a retry.
        self.cleanup().await?;
        result
    }

    /// Runs one workload until completion or an application-owned shutdown signal.
    /// Shutdown takes precedence when both are ready and never returns a success receipt.
    /// The caller must await this method: aborting its task still requires explicit cleanup.
    /// Cleanup failure retains the job in this owner for recovery; it is not safe to discard it.
    pub async fn validate_until_shutdown(
        &mut self,
        prepared: PreparedSandbox,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<ValidationReceipt, SandboxError> {
        let result = tokio::select! {
            biased;
            _ = shutdown => Err(SandboxError::Failed),
            result = self.validate(prepared) => result,
        };
        self.cleanup().await?;
        result
    }

    async fn finish(&mut self) -> Result<ValidationReceipt, SandboxError> {
        let active = self.active.as_mut().ok_or(SandboxError::Failed)?;
        let limits = &active.prepared.limits;
        let output = collect(
            active.child.as_mut().ok_or(SandboxError::Failed)?,
            limits.output_bytes,
            Duration::from_secs(limits.timeout_seconds + 10),
        )
        .await?;
        #[cfg(test)]
        {
            self.last_workload_output = Some(output.clone());
        }
        if !output.success {
            return Err(SandboxError::Failed);
        }
        let name = active.name.clone();
        let state = self
            .control(&[
                "inspect",
                "--type=container",
                "--format",
                "{{json .State}}",
                &name,
            ])
            .await?;
        if !state.success || !valid_exit(&state.stdout) {
            return Err(SandboxError::Failed);
        }
        let active = self.active.as_ref().ok_or(SandboxError::Failed)?;
        Ok(ValidationReceipt {
            schema: "ai-sre/sandbox-validation/v1",
            artifact_digest: active.prepared.artifact_digest.clone(),
            snapshot_digest: active.prepared.snapshot_digest().into(),
            validator_image: active.prepared.image.clone(),
            runtime_digest: self.binary_digest.clone(),
            policy_digest: digest(
                &serde_json::to_vec(&active.prepared.limits).map_err(|_| SandboxError::Failed)?,
            ),
            output_digest: digest(
                format!("{}:{}", digest(&output.stdout), digest(&output.stderr)).as_bytes(),
            ),
            output_bytes: output.stdout.len() + output.stderr.len(),
            completed_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| SandboxError::Failed)?
                .as_secs(),
        })
    }

    /// Terminates and removes only this owner's generated job before releasing its snapshot.
    /// Failure retains ownership and prevents admitting another job on this validator.
    pub async fn cleanup(&mut self) -> Result<(), SandboxError> {
        let Some(active) = self.active.as_mut() else {
            return self.recover_pending().await;
        };
        if let Some(child) = active.child.as_mut() {
            child.start_kill().map_err(|_| SandboxError::Failed)?;
            tokio::time::timeout(Duration::from_secs(10), child.wait())
                .await
                .map_err(|_| SandboxError::Failed)?
                .map_err(|_| SandboxError::Failed)?;
        }
        active.child = None;
        let name = active.name.clone();
        if self.jobs.is_some() {
            let pending = self
                .jobs
                .as_ref()
                .ok_or(SandboxError::Failed)?
                .pending()?
                .ok_or(SandboxError::Failed)?;
            if pending.name != name {
                return Err(SandboxError::Failed);
            }
            self.recover_pending().await?;
            self.active = None;
            return Ok(());
        }
        let output = self
            .control(&["rm", "--force", "--ignore", "--time=0", &name])
            .await?;
        if !output.success {
            return Err(SandboxError::Failed);
        }
        self.active = None;
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStorage {
    graph_root: PathBuf,
    run_root: PathBuf,
    graph_driver_name: String,
    transient_store: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Host {
    os: String,
    cgroup_version: String,
    cgroup_controllers: Vec<String>,
    service_is_remote: bool,
    security: Security,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Security {
    rootless: bool,
    seccomp_enabled: bool,
}
fn valid_host(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Host>(bytes).is_ok_and(|host| {
        host.os == "linux"
            && host.cgroup_version == "v2"
            && !host.service_is_remote
            && host.security.rootless
            && host.security.seccomp_enabled
            && ["cpu", "memory", "pids"].iter().all(|controller| {
                host.cgroup_controllers
                    .iter()
                    .any(|value| value == controller)
            })
    })
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ExitState {
    status: String,
    running: bool,
    paused: bool,
    restarting: bool,
    #[serde(rename = "OOMKilled")]
    oom_killed: bool,
    dead: bool,
    exit_code: i64,
    error: String,
}
fn valid_exit(bytes: &[u8]) -> bool {
    serde_json::from_slice::<ExitState>(bytes).is_ok_and(|state| {
        state.status == "exited"
            && !state.running
            && !state.paused
            && !state.restarting
            && !state.oom_killed
            && !state.dead
            && state.exit_code == 0
            && state.error.is_empty()
    })
}

fn owned_container_id(bytes: &[u8], name: &str, token: &str) -> Option<String> {
    let inspected: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let id = inspected["Id"].as_str()?;
    if id.len() != 64
        || !id.bytes().all(|b| b.is_ascii_hexdigit())
        || inspected["Name"].as_str()? != name
        || inspected["Config"]["Labels"]["io.bodhispace.ai-sre.job"].as_str()? != token
    {
        return None;
    }
    Some(id.to_owned())
}

#[cfg_attr(test, derive(Clone))]
struct Output {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    success: bool,
    code: Option<i32>,
}

async fn collect(
    child: &mut Child,
    limit: usize,
    deadline: Duration,
) -> Result<Output, SandboxError> {
    let mut stdout = child.stdout.take().ok_or(SandboxError::Failed)?;
    let mut stderr = child.stderr.take().ok_or(SandboxError::Failed)?;
    tokio::time::timeout(deadline, async {
        let mut output = Output { stdout: Vec::new(), stderr: Vec::new(), success: false, code: None };
        let (mut out_done, mut err_done) = (false, false);
        let (mut out_buf, mut err_buf) = ([0u8;4096], [0u8;4096]);
        while !out_done || !err_done {
            tokio::select! {
                read = stdout.read(&mut out_buf), if !out_done => {
                    let count = read.map_err(|_| SandboxError::Failed)?;
                    out_done = count == 0;
                    if output.stdout.len() + output.stderr.len() + count > limit { return Err(SandboxError::Failed); }
                    output.stdout.extend_from_slice(&out_buf[..count]);
                },
                read = stderr.read(&mut err_buf), if !err_done => {
                    let count = read.map_err(|_| SandboxError::Failed)?;
                    err_done = count == 0;
                    if output.stdout.len() + output.stderr.len() + count > limit { return Err(SandboxError::Failed); }
                    output.stderr.extend_from_slice(&err_buf[..count]);
                }
            }
        }
        let status = child.wait().await.map_err(|_| SandboxError::Failed)?;
        output.success = status.success();
        output.code = status.code();
        Ok(output)
    }).await.map_err(|_| SandboxError::Failed)?
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    mod incident_flow;
    use super::*;
    use crate::gitops::{
        artifact::ManualRepairRequest,
        receipt::fixture,
        sandbox::{SandboxLimits, SandboxPlan},
        snapshot::RepositorySnapshot,
    };
    use crate::reasoning::storage::JournalStore;
    use std::os::unix::fs::PermissionsExt;

    const HOST: &str = r#"{"os":"linux","cgroupVersion":"v2","cgroupControllers":["cpu","memory","pids"],"serviceIsRemote":false,"security":{"rootless":true,"seccompEnabled":true}}"#;
    const EXIT: &str = r#"{"Status":"exited","Running":false,"Paused":false,"Restarting":false,"OOMKilled":false,"Dead":false,"ExitCode":0,"Error":""}"#;

    #[tokio::test]
    async fn incident_preparation_rebuilds_the_same_candidate_after_restart() {
        use crate::application::manual_repair::{IncidentRepairRequest, prepare_incident_repair};
        // Given qualified deployment evidence and a candidate already recorded for this incident.
        let fixture = Fixture::new("exit 0").await;
        let context = crate::reasoning::journal::JournalContext {
            incident_id: "incident".into(),
            run_id: "run".into(),
        };
        let base = serde_json::from_str::<serde_json::Value>(&fixture.candidate.json()).unwrap()["base_sha"].as_str().unwrap().to_owned();
        let mut reopened = JournalStore::open(fixture.root.join("journal.sqlite")).unwrap();
        let request = IncidentRepairRequest {
            deployment_id: fixture.deployment_receipt.deployment_id(),
            context: &context,
            repository: &fixture.root,
            base: &base,
        };
        // When preparation resumes using only the journal and immutable repository inputs.
        let candidate = prepare_incident_repair(&mut reopened, &request)
            .await
            .unwrap()
            .unwrap();
        // Then it recovers the exact identity rather than minting a candidate from stored JSON authority.
        assert_eq!(
            candidate.artifact_digest(),
            fixture.candidate.artifact_digest()
        );
        let missing = IncidentRepairRequest {
            deployment_id: "unknown",
            repository: Path::new("/nonexistent/repository"),
            ..request
        };
        assert!(
            prepare_incident_repair(&mut reopened, &missing)
                .await
                .unwrap()
                .is_none()
        );
    }

    struct Fixture {
        root: PathBuf,
        source: PathBuf,
        validator: RootlessValidator,
        prepared: Option<PreparedSandbox>,
        store: JournalStore,
        candidate: crate::gitops::artifact::ManualRepairCandidate,
        deployment_receipt: crate::gitops::receipt::ProtectedReceipt,
        handoff_policy: crate::gitops::handoff::HandoffPolicy,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    impl Fixture {
        fn storage_info(&self) -> String {
            storage_info(&self.root)
        }
        fn state_directory(&self) -> PathBuf {
            let path = self.root.join("worker-state");
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            path
        }
        async fn new(run: &str) -> Self {
            Self::with_image(
                run,
                &format!(
                    "ghcr.io/bodhispace-xyz/ai-sre-validator@sha256:{}",
                    "b".repeat(64)
                ),
            )
            .await
        }

        async fn with_image(run: &str, image: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "u9-runtime-fixture-{}-{}",
                std::process::id(),
                NEXT_JOB.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("stacks/utility")).unwrap();
            let source = "services:\n  it-tools:\n    image: ghcr.io/corentinth/it-tools:latest\n";
            fs::write(root.join("stacks/utility/compose.yml"), source).unwrap();
            let git = |args: &[&str]| {
                let result = std::process::Command::new("/usr/bin/git")
                    .arg("-C")
                    .arg(&root)
                    .args(args)
                    .env_clear()
                    .output()
                    .unwrap();
                assert!(result.status.success());
                result.stdout
            };
            git(&["init", "-q"]);
            git(&["add", "."]);
            git(&[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ]);
            let base = String::from_utf8(git(&["rev-parse", "HEAD"])).unwrap();
            let mut receipt = fixture();
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let offset = now - 222;
            receipt.wire.completed_at += offset;
            receipt.wire.policy_declared_at += offset;
            for sample in &mut receipt.wire.samples {
                sample.observed_at += offset;
            }
            let mut store = JournalStore::open(root.join("journal.sqlite")).unwrap();
            store.record_deployment(&receipt, now).unwrap();
            let evidence =
                crate::reasoning::storage::fixture_evidence(&mut store, "incident", "run");
            let candidate = store
                .prepare_manual_candidate(&ManualRepairRequest {
                    deployment_id: receipt.deployment_id(),
                    incident_id: "incident",
                    run_id: "run",
                    expected_base: base.trim(),
                    current_base: base.trim(),
                    evidence_digest: &evidence,
                    source,
                    now,
                })
                .unwrap()
                .unwrap();
            let snapshot = RepositorySnapshot::capture(&root, base.trim(), candidate.image())
                .await
                .unwrap();
            let source = snapshot.source_directory();
            let plan = SandboxPlan::new(
                image,
                SandboxLimits {
                    output_bytes: 1024,
                    ..SandboxLimits::default()
                },
            )
            .unwrap();
            let prepared = plan.prepare("u9-fixture", &candidate, snapshot).unwrap();
            let binary = root.join("fake-podman");
            let script = format!(
                "#!/bin/sh\ncase \"$2\" in\ninfo) if [ \"$4\" = '{{{{json .Store}}}}' ]; then printf '%s' '{}'; else printf '%s' '{HOST}'; fi;;\nrun) /usr/bin/touch '{}'; {run};;\ninspect) printf '%s' '{EXIT}';;\nrm) test ! -f '{}' || exit 1; /usr/bin/touch '{}';;\n*) exit 2;;\nesac\n",
                storage_info(&root),
                root.join("started").display(),
                root.join("refuse-cleanup").display(),
                root.join("cleaned").display()
            );
            fs::write(&binary, &script).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
            // This private test construction bypasses Linux admission, never production validation.
            let validator = RootlessValidator {
                binary,
                binary_digest: digest(script.as_bytes()),
                home: root.clone().into_os_string(),
                runtime_directory: root.clone().into_os_string(),
                active: None,
                jobs: None,
                last_workload_output: None,
            };
            let handoff_policy = crate::gitops::handoff::HandoffPolicy {
                max_validation_age_seconds: 300,
                validator_image: image.into(),
                runtime_digest: validator.binary_digest.clone(),
                sandbox_limits: SandboxLimits {
                    output_bytes: 1024,
                    ..SandboxLimits::default()
                },
            };
            Self {
                root,
                source,
                validator,
                prepared: Some(prepared),
                store,
                candidate,
                deployment_receipt: receipt,
                handoff_policy,
            }
        }
    }

    #[tokio::test]
    async fn manual_handoff_requires_fresh_bound_validation_and_journals_once() {
        use crate::gitops::handoff::ManualHandoffRequest;
        use crate::reasoning::journal::JournalContext;
        // Given a journaled candidate and a successful controlled validator, never a caller success flag.
        let mut fixture = Fixture::new("exit 0").await;
        let receipt = fixture
            .validator
            .validate(fixture.prepared.take().unwrap())
            .await
            .unwrap();
        let now = receipt.completed_at();
        let scope = JournalContext {
            incident_id: "incident".into(),
            run_id: "run".into(),
        };
        let base = serde_json::from_str::<serde_json::Value>(&fixture.candidate.json()).unwrap()["base_sha"].as_str().unwrap().to_owned();
        let request = ManualHandoffRequest {
            candidate: &fixture.candidate,
            validation: &receipt,
            context: &scope,
            current_base: &base,
            now,
            policy: &fixture.handoff_policy,
        };

        // When a fresh handoff is stored twice after current-state checks.
        let handoff = fixture
            .store
            .finalize_manual_repair(&request)
            .unwrap()
            .unwrap();
        let replay = fixture
            .store
            .finalize_manual_repair(&request)
            .unwrap()
            .unwrap();
        fixture.store = JournalStore::open(fixture.root.join("journal.sqlite")).unwrap();
        assert_eq!(
            fixture
                .store
                .finalize_manual_repair(&request)
                .unwrap()
                .unwrap()
                .digest(),
            handoff.digest()
        );

        // Then exactly one handoff fact exists and remote checks remain explicitly unrun.
        assert_eq!(handoff.digest(), replay.digest());
        assert_eq!(fixture.store.journal().entries().len(), 4);
        let efficiency = fixture.store.efficiency_projection(&scope);
        assert_eq!(efficiency.manual_repair_preparations, 1);
        assert_eq!(efficiency.manual_repair_validated_handoffs, 1);
        assert!(handoff.json().contains("operator_review_required"));
        assert!(handoff.json().contains("not_run"));
        // Operator retrieval after restart delivers the exact journaled handoff as historical data.
        let stored = fixture
            .store
            .manual_repair_artifact(&fixture.candidate.artifact_digest())
            .unwrap()
            .unwrap();
        assert_eq!(stored.handoff_json.as_deref(), Some(handoff.json()));
        // Reading historical data does not close operator wait; acknowledgement is a separate action.
        assert_eq!(
            fixture
                .store
                .efficiency_projection(&scope)
                .manual_handoff_acknowledgements,
            0
        );
        assert!(
            !fixture
                .store
                .acknowledge_manual_handoff("missing", 501, 20, now + 10)
                .unwrap()
        );
        let fault = rusqlite::Connection::open(fixture.root.join("journal.sqlite")).unwrap();
        fault.execute_batch("CREATE TRIGGER reject_ack BEFORE INSERT ON journal_events BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
        assert!(
            fixture
                .store
                .acknowledge_manual_handoff(&handoff.digest(), 501, 20, now + 10)
                .is_err()
        );
        assert!(
            fixture
                .store
                .manual_handoff_acknowledgement(&handoff.digest())
                .unwrap()
                .is_none()
        );
        fault.execute_batch("DROP TRIGGER reject_ack;").unwrap();
        for at in [now + 10, now + 20] {
            assert!(
                fixture
                    .store
                    .acknowledge_manual_handoff(&handoff.digest(), 501, 20, at)
                    .unwrap()
            );
        }
        fixture.store = JournalStore::open(fixture.root.join("journal.sqlite")).unwrap();
        let progress = fixture.store.efficiency_projection(&scope);
        assert_eq!(progress.manual_handoff_acknowledgements, 1);
        assert_eq!(progress.manual_handoff_wait_seconds, 10);
        assert_eq!(progress.manual_handoff_timed_acknowledgements, 1);
        assert_eq!(progress.human_wait_ms, 0);
        let ack = fixture
            .store
            .manual_handoff_acknowledgement(&handoff.digest())
            .unwrap()
            .unwrap();
        assert!(matches!(
            ack,
            crate::reasoning::journal::JournalEvent::ManualHandoffAcknowledged {
                operator_uid: 501,
                operator_gid: 20,
                ..
            }
        ));
        let foreign_scope = JournalContext {
            incident_id: "other-incident".into(),
            run_id: "run".into(),
        };
        assert!(
            fixture
                .store
                .finalize_manual_repair(&ManualHandoffRequest {
                    context: &foreign_scope,
                    ..request
                })
                .unwrap()
                .is_none()
        );
        assert!(
            fixture
                .store
                .finalize_manual_repair(&ManualHandoffRequest {
                    current_base: &"f".repeat(40),
                    ..request
                })
                .unwrap()
                .is_none()
        );
        assert!(
            fixture
                .store
                .finalize_manual_repair(&ManualHandoffRequest {
                    now: now + 301,
                    ..request
                })
                .unwrap()
                .is_none()
        );
        assert!(
            fixture
                .store
                .finalize_manual_repair(&ManualHandoffRequest {
                    now: now - 1,
                    ..request
                })
                .unwrap()
                .is_none()
        );
        // When new durable evidence changes the run after validation, even replay must fail closed.
        fixture
            .store
            .append_scoped(
                crate::reasoning::journal::JournalEvent::EvidenceCommitted {
                    evidence_id: "evidence-0002".into(),
                    source: crate::reasoning::evidence::EvidenceSource::Health,
                    content_digest: Some(digest(b"later synthetic evidence")),
                    at_ms: 2,
                },
                Some(&scope),
            )
            .unwrap();
        assert!(
            fixture
                .store
                .finalize_manual_repair(&request)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn handoff_audit_failure_rolls_back_the_operator_artifact() {
        use crate::gitops::handoff::ManualHandoffRequest;
        use crate::reasoning::journal::JournalContext;
        // Given a valid controlled result and a real SQLite trigger rejecting its audit fact.
        let mut fixture = Fixture::new("exit 0").await;
        let receipt = fixture
            .validator
            .validate(fixture.prepared.take().unwrap())
            .await
            .unwrap();
        let connection = rusqlite::Connection::open(fixture.root.join("journal.sqlite")).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_handoff BEFORE INSERT ON journal_events WHEN json_type(NEW.event_json,'$.ManualRepairValidated')='object' BEGIN SELECT RAISE(FAIL,'injected handoff audit failure'); END;").unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&fixture.candidate.json()).unwrap();
        let scope = JournalContext {
            incident_id: "incident".into(),
            run_id: "run".into(),
        };
        let request = ManualHandoffRequest {
            candidate: &fixture.candidate,
            validation: &receipt,
            context: &scope,
            current_base: metadata["base_sha"].as_str().unwrap(),
            now: receipt.completed_at(),
            policy: &fixture.handoff_policy,
        };

        // When finalization cannot commit its audit entry.
        assert!(fixture.store.finalize_manual_repair(&request).is_err());

        // Then no deliverable artifact survives; after recovery one retry commits both records.
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM manual_repair_handoffs", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(fixture.store.journal().entries().len(), 3);
        connection
            .execute_batch("DROP TRIGGER reject_handoff")
            .unwrap();
        assert!(
            fixture
                .store
                .finalize_manual_repair(&request)
                .unwrap()
                .is_some()
        );
        assert_eq!(fixture.store.journal().entries().len(), 4);
    }

    #[tokio::test]
    async fn handoff_rechecks_qualification_even_when_validation_is_fresh() {
        use crate::gitops::handoff::ManualHandoffRequest;
        use crate::reasoning::journal::JournalContext;
        // Given a successful validation whose age policy outlives deployment qualification.
        let mut fixture = Fixture::new("exit 0").await;
        fixture.handoff_policy.max_validation_age_seconds = 3600;
        let receipt = fixture
            .validator
            .validate(fixture.prepared.take().unwrap())
            .await
            .unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&fixture.candidate.json()).unwrap();
        let scope = JournalContext {
            incident_id: "incident".into(),
            run_id: "run".into(),
        };
        let request = ManualHandoffRequest {
            candidate: &fixture.candidate,
            validation: &receipt,
            context: &scope,
            current_base: metadata["base_sha"].as_str().unwrap(),
            now: receipt.completed_at(),
            policy: &fixture.handoff_policy,
        };

        // When the original qualification expires, a still-fresh validation cannot replace it.
        assert!(
            fixture
                .store
                .finalize_manual_repair(&ManualHandoffRequest {
                    now: request.now + 601,
                    ..request
                })
                .unwrap()
                .is_none()
        );

        // When the producer instead revokes the deployment before either freshness limit expires.
        fixture.deployment_receipt.wire.revoked = true;
        assert!(
            !fixture
                .store
                .record_deployment(&fixture.deployment_receipt, request.now)
                .unwrap()
        );

        // Then the previously validated candidate cannot become an operator artifact.
        assert!(
            fixture
                .store
                .finalize_manual_repair(&request)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn handoff_requires_the_independently_enrolled_image_runtime_and_limits() {
        use crate::gitops::handoff::ManualHandoffRequest;
        use crate::reasoning::journal::JournalContext;
        // Given a sealed result from a controlled fixture, not an enrolled substantive validator.
        let mut fixture = Fixture::new("exit 0").await;
        let receipt = fixture
            .validator
            .validate(fixture.prepared.take().unwrap())
            .await
            .unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&fixture.candidate.json()).unwrap();
        let scope = JournalContext {
            incident_id: "incident".into(),
            run_id: "run".into(),
        };
        let request = ManualHandoffRequest {
            candidate: &fixture.candidate,
            validation: &receipt,
            context: &scope,
            current_base: metadata["base_sha"].as_str().unwrap(),
            now: receipt.completed_at(),
            policy: &fixture.handoff_policy,
        };

        // When deployment configuration enrolls another image, runtime, or otherwise valid limit set.
        let mut wrong_image = fixture.handoff_policy.clone();
        wrong_image.validator_image = format!(
            "ghcr.io/bodhispace-xyz/ai-sre-validator@sha256:{}",
            "c".repeat(64)
        );
        let mut wrong_runtime = fixture.handoff_policy.clone();
        wrong_runtime.runtime_digest = format!("sha256:{}", "c".repeat(64));
        let mut wrong_limits = fixture.handoff_policy.clone();
        wrong_limits.sandbox_limits.memory_mib = 1024;
        for policy in [&wrong_image, &wrong_runtime, &wrong_limits] {
            // Then syntactic pinning and a successful process are not sufficient authority for handoff.
            assert!(
                fixture
                    .store
                    .finalize_manual_repair(&ManualHandoffRequest { policy, ..request })
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(fixture.store.journal().entries().len(), 3);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires a protected rootless Podman and preloaded containment-test image"]
    async fn linux_rootless_containment_uses_production_admission_and_cleanup() {
        // Given an explicitly pinned local runtime and a containment-only fixture image.
        // The protected deployment fixture is synthetic: this test does not qualify a homelab image.
        let image =
            std::env::var("U9_CONTAINMENT_IMAGE").expect("set the preloaded fixture digest");
        let runtime_digest =
            std::env::var("U9_RUNTIME_DIGEST").expect("set the protected binary digest");
        let mut fixture = Fixture::with_image("exit 0", &image).await;
        let mut validator = RootlessValidator::connect_with_state(
            Path::new("/usr/local/bin/podman"),
            &runtime_digest,
            &fixture.state_directory(),
        )
        .await
        .expect("production runtime admission must pass");
        let prepared = fixture.prepared.take().unwrap();
        let artifact_digest = prepared.artifact_digest().to_owned();

        // When the actual rootless engine runs the resource and mount assertions.
        let receipt = validator
            .validate(prepared)
            .await
            .expect("containment assertions and cleanup must pass");

        // Then the receipt binds this fixture, and no active job or mounted snapshot remains.
        assert_eq!(receipt.artifact_digest(), artifact_digest);
        assert!(validator.active.is_none());
        assert!(!fixture.source.exists());
        validator.cleanup().await.unwrap();

        // Given independent enrollment for another image, successful containment is not homelab CI.
        let mut enrollment = fixture.handoff_policy.clone();
        enrollment.runtime_digest = runtime_digest;
        enrollment.validator_image = format!(
            "ghcr.io/bodhispace-xyz/ai-sre-validator@sha256:{}",
            "c".repeat(64)
        );
        assert_ne!(enrollment.validator_image, image);
        let metadata: serde_json::Value = serde_json::from_str(&fixture.candidate.json()).unwrap();
        let scope = crate::reasoning::journal::JournalContext {
            incident_id: "incident".into(),
            run_id: "run".into(),
        };
        let request = crate::gitops::handoff::ManualHandoffRequest {
            candidate: &fixture.candidate,
            validation: &receipt,
            context: &scope,
            current_base: metadata["base_sha"].as_str().unwrap(),
            now: receipt.completed_at(),
            policy: &enrollment,
        };
        // Then the real sealed fixture result still cannot produce a validated operator handoff.
        assert!(
            fixture
                .store
                .finalize_manual_repair(&request)
                .unwrap()
                .is_none()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires an isolated rootless runtime, offline validator image and homelab Git fixture"]
    async fn linux_offline_homelab_checks_use_production_runner() {
        // Given an archived homelab tree committed in a disposable repository and a preloaded image.
        // Qualification is synthetic; successful checks do not qualify a production deployment.
        let repository = PathBuf::from(std::env::var("U9_HOMELAB_REPOSITORY").unwrap());
        let image = std::env::var("U9_OFFLINE_IMAGE").unwrap();
        let runtime_digest = std::env::var("U9_RUNTIME_DIGEST").unwrap();
        let mut fixture = Fixture::new("exit 0").await;
        let head = std::process::Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&repository)
            .args(["rev-parse", "HEAD"])
            .env_clear()
            .output()
            .unwrap();
        assert!(head.status.success());
        let base = String::from_utf8(head.stdout).unwrap();
        let snapshot =
            RepositorySnapshot::capture(&repository, base.trim(), fixture.candidate.image())
                .await
                .unwrap();
        let evidence =
            crate::reasoning::storage::fixture_evidence(&mut fixture.store, "offline", "offline");
        let candidate = fixture
            .store
            .prepare_manual_candidate(&ManualRepairRequest {
                deployment_id: fixture.deployment_receipt.deployment_id(),
                incident_id: "offline",
                run_id: "offline",
                expected_base: base.trim(),
                current_base: base.trim(),
                evidence_digest: &evidence,
                source: snapshot.original_source(),
                now: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            })
            .unwrap()
            .unwrap();
        let source = snapshot.source_directory();
        if std::env::var("U9_WORKER_SSH").as_deref() == Ok("1") {
            let policy = crate::gitops::handoff::HandoffPolicy {
                max_validation_age_seconds: 300,
                validator_image: image,
                runtime_digest,
                sandbox_limits: SandboxLimits::default(),
            };
            let ssh_config = || SshWorkerConfig {
                host: "127.0.0.1".into(),
                user: "validator".into(),
                port: 22222,
                identity_file: "/home/validator/u9-ssh-client/key".into(),
                known_hosts: "/home/validator/u9-ssh-client/known_hosts".into(),
            };
            let mut worker = SshValidator::new(ssh_config()).unwrap();
            // When the application validates through the real SSH forced-command worker.
            let context = crate::reasoning::journal::JournalContext {
                incident_id: "offline".into(),
                run_id: "offline".into(),
            };
            let handoff = crate::application::manual_repair::validate_manual_repair(
                &mut fixture.store,
                &mut worker,
                crate::application::manual_repair::ManualValidationRequest {
                    candidate: &candidate,
                    context: &context,
                    repository: &repository,
                    base: base.trim(),
                    policy: &policy,
                },
            )
            .await
            .unwrap()
            .unwrap();
            // Then only the bound, journaled operator-review artifact is ready; no publication occurred.
            let wire: serde_json::Value = serde_json::from_str(handoff.json()).unwrap();
            assert_eq!(wire["candidate_digest"], candidate.artifact_digest());
            assert_eq!(wire["remote_checks"], "not_run");
            assert_eq!(wire["status"], "operator_review_required");
            // Given a fresh request but a deliberately wrong independently enrolled host key.
            let job = RemoteJob::new(
                &candidate,
                &snapshot,
                &policy,
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            )
            .unwrap();
            let mut wrong = ssh_config();
            wrong.known_hosts = "/home/validator/u9-ssh-client/wrong_hosts".into();
            assert!(
                SshValidator::new(wrong)
                    .unwrap()
                    .validate(&job, &policy)
                    .await
                    .is_err()
            );
            // When the same job is sent to the correctly authenticated worker, it runs once only.
            assert!(worker.validate(&job, &policy).await.is_ok());
            assert!(worker.validate(&job, &policy).await.is_err());
            return;
        }
        let plan = SandboxPlan::new(&image, SandboxLimits::default()).unwrap();
        let prepared = plan
            .prepare("u9-offline-homelab", &candidate, snapshot)
            .unwrap();
        let mut validator = RootlessValidator::connect_with_state(
            Path::new("/usr/local/bin/podman"),
            &runtime_digest,
            &fixture.state_directory(),
        )
        .await
        .unwrap();

        // When the production adapter runs full Make CI without networking or host credentials.
        let result = validator
            .validate_until_shutdown(prepared, std::future::pending())
            .await;
        // Then a bound result requires successful checks and removal of both container and snapshot.
        // A separate deliberately invalid Git fixture uses the same test to prove failure propagation.
        if std::env::var("U9_EXPECT_VALIDATION_FAILURE").as_deref() == Ok("1") {
            assert!(result.is_err());
            assert!(
                invalid_fixture_was_rejected(&validator),
                "the expected OpenTofu fixture rejection must be observed"
            );
        } else {
            let receipt = result.expect("substantive offline homelab validation must pass");
            assert_eq!(receipt.artifact_digest(), candidate.artifact_digest());
        }
        assert!(validator.active.is_none());
        assert!(!source.exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires isolated rootless Podman and a preloaded image containing /bin/sleep"]
    async fn linux_restart_recovery_preserves_unrelated_container() {
        // Given real rootless containers: one recorded interrupted job and one unrelated workload.
        let image = std::env::var("U9_CONTAINMENT_IMAGE").unwrap();
        let runtime_digest = std::env::var("U9_RUNTIME_DIGEST").unwrap();
        let fixture = Fixture::with_image("exit 0", &image).await;
        let state = fixture.state_directory();
        let mut owner = RootlessValidator::connect_with_state(
            Path::new("/usr/local/bin/podman"),
            &runtime_digest,
            &state,
        )
        .await
        .unwrap();
        let job = owner
            .jobs
            .as_mut()
            .unwrap()
            .begin(
                "artifact",
                "snapshot",
                &image,
                fixture.source.parent().unwrap(),
            )
            .unwrap();
        let foreign_name = format!("{}-other", job.name);
        let mut ids = Vec::new();
        for (name, token) in [
            (&job.name, job.token.as_str()),
            (&foreign_name, "unrelated"),
        ] {
            let output = owner
                .control(&[
                    "run",
                    "--detach",
                    "--pull=never",
                    "--network=none",
                    "--read-only",
                    "--cap-drop=ALL",
                    "--security-opt=no-new-privileges",
                    "--user=65532:65532",
                    "--memory=128m",
                    "--pids-limit=32",
                    "--timeout=60",
                    "--name",
                    name,
                    "--label",
                    &format!("io.bodhispace.ai-sre.job={token}"),
                    "--entrypoint=/bin/sleep",
                    &image,
                    "60",
                ])
                .await
                .unwrap();
            assert!(output.success);
            ids.push(String::from_utf8(output.stdout).unwrap().trim().to_owned());
        }
        drop(owner);

        // When another admitted owner reopens the journal, as after an interrupted process.
        let mut recovered = RootlessValidator::connect_with_state(
            Path::new("/usr/local/bin/podman"),
            &runtime_digest,
            &state,
        )
        .await
        .unwrap();
        // Then recovery removes only the recorded workload and its snapshot, leaving its neighbor alive.
        assert_eq!(
            recovered
                .control(&["container", "exists", &ids[0]])
                .await
                .unwrap()
                .code,
            Some(1)
        );
        assert_eq!(
            recovered
                .control(&["container", "exists", &ids[1]])
                .await
                .unwrap()
                .code,
            Some(0)
        );
        assert!(!fixture.source.exists());
        assert!(
            recovered
                .jobs
                .as_ref()
                .unwrap()
                .pending()
                .unwrap()
                .is_none()
        );
        // The test itself removes the unrelated fixture by its captured immutable ID.
        assert!(
            recovered
                .control(&["rm", "--force", "--ignore", "--time=0", &ids[1]])
                .await
                .unwrap()
                .success
        );
    }

    fn storage_info(root: &Path) -> String {
        serde_json::json!({"graphRoot": root, "runRoot": root, "graphDriverName": "overlay", "transientStore": false}).to_string()
    }

    #[tokio::test]
    async fn confirmed_spawn_failure_releases_intent_and_allows_another_launch() {
        // Given a runtime that loses execute permission after admission but before workload spawn.
        let mut fixture = Fixture::new("exit 125").await;
        let original = fs::read_to_string(&fixture.validator.binary).unwrap();
        let script = original.replace(
            &format!("else printf '%s' '{HOST}'; fi"),
            &format!("else /bin/chmod 644 \"$0\"; printf '%s' '{HOST}'; fi"),
        );
        fs::write(&fixture.validator.binary, &script).unwrap();
        fixture.validator.binary_digest = digest(script.as_bytes());
        fixture.validator.jobs = Some(JobStore::open(&fixture.state_directory()).unwrap());
        // When spawn returns an OS error without starting a workload process.
        assert!(
            fixture
                .validator
                .validate(fixture.prepared.take().unwrap())
                .await
                .is_err()
        );
        // Then no launch intent or snapshot is retained for a process that never started.
        assert!(
            fixture
                .validator
                .jobs
                .as_ref()
                .unwrap()
                .pending()
                .unwrap()
                .is_none()
        );
        assert!(!fixture.source.exists());
        assert!(!fixture.root.join("started").exists());
        // Given restored executable permissions and a new prepared workload.
        fs::write(&fixture.validator.binary, &original).unwrap();
        fs::set_permissions(&fixture.validator.binary, fs::Permissions::from_mode(0o755)).unwrap();
        fixture.validator.binary_digest = digest(original.as_bytes());
        let mut next = Fixture::new("exit 0").await;
        // When validation is retried, the new workload reaches the runtime (which deliberately exits 125).
        assert!(
            fixture
                .validator
                .validate(next.prepared.take().unwrap())
                .await
                .is_err()
        );
        assert!(fixture.root.join("started").exists());
    }

    #[tokio::test]
    async fn application_retry_rejects_uncertain_and_completed_attempts_before_dispatch() {
        use crate::reasoning::storage::ManualValidationState;
        // Given an already reserved candidate whose request may have reached the remote worker.
        let mut fixture = Fixture::new("exit 0").await;
        let artifact = fixture.candidate.artifact_digest();
        assert!(
            fixture
                .store
                .reserve_manual_validation(&artifact, r#"{"nonce":"original"}"#)
                .unwrap()
        );
        let mut worker = SshValidator::new(SshWorkerConfig {
            host: "worker.invalid".into(),
            user: "validator".into(),
            port: 22,
            identity_file: "/nonexistent/key".into(),
            known_hosts: "/nonexistent/hosts".into(),
        })
        .unwrap();
        let context = crate::reasoning::journal::JournalContext {
            incident_id: "incident".into(),
            run_id: "run".into(),
        };
        // When a restarted application retries with paths that would otherwise fail before SSH.
        for expected in [
            ManualValidationState::Uncertain,
            ManualValidationState::Validated,
        ] {
            let mut reopened = JournalStore::open(fixture.root.join("journal.sqlite")).unwrap();
            assert_eq!(
                reopened
                    .manual_validation_attempt(&artifact)
                    .unwrap()
                    .unwrap()
                    .state,
                expected
            );
            let result = crate::application::manual_repair::validate_manual_repair(
                &mut reopened,
                &mut worker,
                crate::application::manual_repair::ManualValidationRequest {
                    candidate: &fixture.candidate,
                    context: &context,
                    repository: Path::new("/nonexistent/repository"),
                    base: "invalid",
                    policy: &fixture.handoff_policy,
                },
            )
            .await;
            // Then the durable attempt blocks dispatch regardless of its known/unknown result.
            assert!(matches!(result, Err(SandboxError::AttemptExists)));
            let progress = reopened.efficiency_projection(&context);
            assert_eq!(progress.manual_validation_reservations, 1);
            assert_eq!(
                progress.manual_validation_receipts,
                u64::from(expected == ManualValidationState::Validated)
            );
            assert_eq!(
                reopened
                    .manual_validation_attempt(&artifact)
                    .unwrap()
                    .unwrap()
                    .cleanup_status(),
                if expected == ManualValidationState::Validated {
                    "confirmed_by_receipt"
                } else {
                    "unknown"
                }
            );
            assert_eq!(
                progress.manual_validation_response_ms,
                if expected == ManualValidationState::Validated {
                    23
                } else {
                    0
                }
            );
            assert_eq!(
                progress.manual_validation_timed_receipts,
                u64::from(expected == ManualValidationState::Validated)
            );
            if expected == ManualValidationState::Uncertain {
                let receipt = fixture
                    .validator
                    .validate(fixture.prepared.take().unwrap())
                    .await
                    .unwrap();
                let wire = fixture
                    .store
                    .manual_validation_attempt(&artifact)
                    .unwrap()
                    .unwrap()
                    .request_json;
                // A delayed result must not complete a different request's reservation.
                assert!(
                    fixture
                        .store
                        .complete_manual_validation(
                            &receipt,
                            "different request",
                            Some(Duration::from_millis(23))
                        )
                        .is_err()
                );
                // Given a storage fault after receipt authentication but before its fact can commit.
                let fault =
                    rusqlite::Connection::open(fixture.root.join("journal.sqlite")).unwrap();
                fault.execute_batch("CREATE TRIGGER reject_receipt_fact BEFORE INSERT ON journal_events BEGIN SELECT RAISE(ABORT, 'injected journal failure'); END;").unwrap();
                // When persisting the bound receipt fails, it must not mark the attempt validated.
                assert!(
                    fixture
                        .store
                        .complete_manual_validation(
                            &receipt,
                            &wire,
                            Some(Duration::from_millis(23))
                        )
                        .is_err()
                );
                assert_eq!(
                    fixture
                        .store
                        .manual_validation_attempt(&artifact)
                        .unwrap()
                        .unwrap()
                        .state,
                    ManualValidationState::Uncertain
                );
                assert_eq!(
                    fixture
                        .store
                        .efficiency_projection(&context)
                        .manual_validation_receipts,
                    0
                );
                fault
                    .execute_batch("DROP TRIGGER reject_receipt_fact;")
                    .unwrap();
                // Then a later successful commit records exactly one receipt and survives reopening.
                fixture
                    .store
                    .complete_manual_validation(&receipt, &wire, Some(Duration::from_millis(23)))
                    .unwrap();
                assert!(
                    fixture
                        .store
                        .complete_manual_validation(
                            &receipt,
                            &wire,
                            Some(Duration::from_millis(23))
                        )
                        .is_err()
                );
                assert_eq!(
                    fixture
                        .store
                        .efficiency_projection(&context)
                        .manual_validation_receipts,
                    1
                );
            }
        }
    }

    #[tokio::test]
    async fn recovery_refuses_a_changed_runtime_before_accepting_container_absence() {
        // Given durable intent whose container was observed in the original runtime.
        let mut fixture = Fixture::new("exit 0").await;
        let mut jobs = JobStore::open(&fixture.state_directory()).unwrap();
        jobs.bind_runtime(&fixture.validator.runtime_identity().await.unwrap())
            .unwrap();
        let job = jobs
            .begin(
                "artifact",
                "snapshot",
                "image",
                fixture.source.parent().unwrap(),
            )
            .unwrap();
        jobs.remember_container(&job, &"a".repeat(64)).unwrap();
        fixture.validator.jobs = Some(jobs);
        // When the runtime reports different storage and says that container is absent.
        let other = fixture.root.join("other-storage");
        fs::create_dir(&other).unwrap();
        let script = format!(
            "#!/bin/sh\ncase \"$2\" in\ninfo) printf '%s' '{}';;\ncontainer) exit 1;;\n*) exit 125;;\nesac\n",
            storage_info(&other)
        );
        fs::write(&fixture.validator.binary, &script).unwrap();
        fixture.validator.binary_digest = digest(script.as_bytes());
        // Then cleanup fails without discarding the journal or deleting its snapshot.
        assert!(fixture.validator.cleanup().await.is_err());
        assert!(
            fixture
                .validator
                .jobs
                .as_ref()
                .unwrap()
                .pending()
                .unwrap()
                .is_some()
        );
        assert!(fixture.source.exists());
    }

    #[tokio::test]
    async fn recovery_requires_recorded_ownership_and_removes_by_immutable_id() {
        // Given durable intent left by a previous owner and several possible runtime observations.
        for (exists_code, matching_label, known_id, recoverable) in [
            (0, true, false, true),
            (0, false, false, false),
            (1, false, false, false),
            (1, false, true, true),
            (125, false, false, false),
        ] {
            let mut fixture = Fixture::new("exit 0").await;
            let state = fixture.state_directory();
            let mut jobs = JobStore::open(&state).unwrap();
            jobs.bind_runtime(&fixture.validator.runtime_identity().await.unwrap())
                .unwrap();
            let job = jobs
                .begin(
                    "artifact",
                    "snapshot",
                    "image",
                    fixture.source.parent().unwrap(),
                )
                .unwrap();
            let id = "a".repeat(64);
            if known_id {
                jobs.remember_container(&job, &id).unwrap();
            }
            drop(jobs);
            fixture.validator.jobs = Some(JobStore::open(&state).unwrap());
            let identity = serde_json::json!({"Id": id, "Name": job.name, "Config": {"Labels": {
                "io.bodhispace.ai-sre.job": if matching_label { job.token.as_str() } else { "foreign" }
            }}});
            let removed = fixture.root.join("removed-arguments");
            let script = format!(
                "#!/bin/sh\ncase \"$2\" in\ninfo) printf '%s' '{}';;\ncontainer) exit {exists_code};;\ninspect) printf '%s' '{identity}';;\nrm) printf '%s\\n' \"$@\" > '{}';;\n*) exit 125;;\nesac\n",
                fixture.storage_info(),
                removed.display()
            );
            fs::write(&fixture.validator.binary, &script).unwrap();
            fixture.validator.binary_digest = digest(script.as_bytes());

            // When the new owner reconciles the pending launch, without scanning other containers.
            let result = fixture.validator.cleanup().await;
            // Then foreign ownership/runtime failure blocks admission and retains the recovery record.
            assert_eq!(result.is_ok(), recoverable);
            assert_eq!(
                fixture
                    .validator
                    .jobs
                    .as_ref()
                    .unwrap()
                    .pending()
                    .unwrap()
                    .is_none(),
                recoverable
            );
            if exists_code == 0 && matching_label {
                let arguments = fs::read_to_string(removed).unwrap();
                assert!(arguments.lines().any(|arg| arg == id));
                assert!(!arguments.lines().any(|arg| arg == job.name));
            } else {
                assert!(!removed.exists());
            }
        }
    }

    #[test]
    fn host_and_exit_claims_require_every_enforcement_field() {
        // Given complete runtime claims, and variants with missing or unsafe enforcement.
        assert!(valid_host(HOST.as_bytes()));
        assert!(valid_exit(EXIT.as_bytes()));
        // When rootless operation, controllers, or a clean exit cannot be established.
        for bad in [
            "{}".to_owned(),
            HOST.replace("\"rootless\":true", "\"rootless\":false"),
            HOST.replace("\"memory\",", ""),
            HOST.replace("\"serviceIsRemote\":false", "\"serviceIsRemote\":true"),
        ] {
            // Then admission fails closed rather than filling missing fields with permissive defaults.
            assert!(!valid_host(bad.as_bytes()));
        }
        assert!(!valid_exit(
            EXIT.replace("\"OOMKilled\":false", "\"OOMKilled\":true")
                .as_bytes()
        ));
        assert!(!valid_exit(b"{}"));
    }

    #[tokio::test]
    async fn oversized_mount_configuration_is_rejected_before_runtime_execution() {
        // Given a comments-only mount configuration larger than the admission read budget.
        let fixture = Fixture::new("exit 0").await;
        let config = fixture.root.join(".config/containers");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("mounts.conf"),
            format!("#{}", "x".repeat(CONTROL_LIMIT)),
        )
        .unwrap();

        // When admission checks default host mounts before starting the validator.
        let result = fixture.validator.check_mount_defaults();

        // Then even harmless-looking content cannot bypass the bounded configuration read.
        assert!(result.is_err());
        assert!(!fixture.root.join("started").exists());
    }

    #[tokio::test]
    async fn successful_process_receipt_is_bound_and_waits_for_cleanup() {
        // Given an exact candidate and a controlled process protocol, not a real container.
        let mut fixture = Fixture::new("printf validated").await;
        let prepared = fixture.prepared.take().unwrap();
        let expected = prepared.artifact_digest().to_owned();
        let snapshot = prepared.snapshot_digest().to_owned();
        // When the process exits cleanly and removal succeeds.
        let receipt = fixture
            .validator
            .validate_until_shutdown(prepared, std::future::pending())
            .await
            .unwrap();
        // Then the receipt names the exact inputs and source is released only after cleanup.
        assert_eq!(receipt.artifact_digest(), expected);
        assert_eq!(receipt.snapshot_digest(), snapshot);
        assert_eq!(receipt.output_bytes, 9);
        assert!(fixture.root.join("cleaned").exists());
        assert!(!fixture.source.exists());
    }

    #[tokio::test]
    async fn negative_acceptance_requires_the_expected_check_diagnostic() {
        // Given both a silent infrastructure failure and the expected fixture rejection.
        for (run, expected) in [
            ("exit 125", false),
            (
                "printf 'invalid-fixture.tf: Invalid block definition' >&2; exit 2",
                true,
            ),
        ] {
            let mut fixture = Fixture::new(run).await;
            // When the bounded workload fails through the normal validation and cleanup path.
            assert!(
                fixture
                    .validator
                    .validate(fixture.prepared.take().unwrap())
                    .await
                    .is_err()
            );
            // Then only the intended repository diagnostic can satisfy negative acceptance.
            assert_eq!(invalid_fixture_was_rejected(&fixture.validator), expected);
        }
    }

    fn invalid_fixture_was_rejected(validator: &RootlessValidator) -> bool {
        validator
            .last_workload_output
            .as_ref()
            .is_some_and(|output| {
                let diagnostic = String::from_utf8_lossy(&output.stderr);
                !output.success
                    && diagnostic.contains("invalid-fixture.tf")
                    && diagnostic.contains("Invalid block definition")
            })
    }

    #[tokio::test]
    async fn excessive_output_and_nonzero_exit_never_yield_receipts() {
        // Given workloads that exceed output bounds or fail without producing output.
        for run in [
            "i=0; while [ $i -lt 2048 ]; do printf x; i=$((i+1)); done",
            "i=0; while [ $i -lt 600 ]; do printf x; printf y >&2; i=$((i+1)); done",
            "exit 7",
        ] {
            let mut fixture = Fixture::new(run).await;
            // When execution fails its output or exit-status contract.
            let result = fixture
                .validator
                .validate(fixture.prepared.take().unwrap())
                .await;
            // Then no receipt exists and cleanup still releases the captured source.
            assert!(result.is_err());
            assert!(!fixture.source.exists());
            assert!(fixture.root.join("cleaned").exists());
        }
    }

    #[tokio::test]
    async fn pending_shutdown_prevents_a_new_workload_from_starting() {
        // Given a prepared candidate and a shutdown signal that is already ready.
        let mut fixture = Fixture::new("printf validated").await;
        // When the supervised validation method receives both inputs.
        let result = fixture
            .validator
            .validate_until_shutdown(fixture.prepared.take().unwrap(), std::future::ready(()))
            .await;
        // Then shutdown wins before any child starts and releases the unused snapshot.
        assert!(result.is_err());
        assert!(!fixture.root.join("started").exists());
        assert!(!fixture.source.exists());
        assert!(fixture.validator.active.is_none());
    }

    #[tokio::test]
    async fn shutdown_waits_for_cleanup_and_retains_failed_cleanup_for_retry() {
        // Given an active workload, with either functioning or temporarily failing cleanup.
        for cleanup_fails in [false, true] {
            let mut fixture = Fixture::new("exec /bin/sleep 30").await;
            if cleanup_fails {
                fs::write(fixture.root.join("refuse-cleanup"), "fixture").unwrap();
            }
            let started = fixture.root.join("started");
            let shutdown = async {
                while !started.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            // When shutdown is requested only after the workload starts.
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                fixture
                    .validator
                    .validate_until_shutdown(fixture.prepared.take().unwrap(), shutdown),
            )
            .await
            .unwrap();
            // Then no receipt escapes, and successful shutdown has joined and removed the workload.
            assert!(result.is_err());
            assert_eq!(fixture.validator.active.is_some(), cleanup_fails);
            assert_eq!(fixture.source.exists(), cleanup_fails);
            if cleanup_fails {
                // Given restored runtime access, the owner can retry without losing the snapshot.
                fs::remove_file(fixture.root.join("refuse-cleanup")).unwrap();
                fixture.validator.cleanup().await.unwrap();
            }
            assert!(!fixture.source.exists());
            assert!(fixture.root.join("cleaned").exists());
        }
    }

    #[tokio::test]
    async fn cancellation_retains_the_job_until_cleanup_succeeds() {
        // Given a process that stays alive and a temporarily unavailable cleanup operation.
        let mut fixture = Fixture::new("exec /bin/sleep 30").await;
        fs::write(fixture.root.join("refuse-cleanup"), "fixture").unwrap();
        // When the validation future is cancelled after spawning its workload.
        let started = fixture.root.join("started");
        let observe_start = async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            _ = fixture.validator.validate(fixture.prepared.take().unwrap()) => panic!("workload ended before cancellation"),
            observed = tokio::time::timeout(Duration::from_secs(5), observe_start) => observed.unwrap(),
        }
        // Then the owner retains the snapshot, and failed cleanup cannot discard the job.
        assert!(fixture.validator.active.is_some());
        assert!(fixture.source.exists());
        assert!(fixture.validator.cleanup().await.is_err());
        assert!(fixture.validator.active.is_some());
        assert!(fixture.source.exists());
        fs::remove_file(fixture.root.join("refuse-cleanup")).unwrap();
        fixture.validator.cleanup().await.unwrap();
        assert!(fixture.validator.active.is_none());
        assert!(!fixture.source.exists());
    }

    #[tokio::test]
    async fn successful_exit_cannot_override_a_cleanup_failure() {
        // Given a clean workload exit but a runtime that refuses container removal.
        let mut fixture = Fixture::new("printf validated").await;
        fs::write(fixture.root.join("refuse-cleanup"), "fixture").unwrap();
        // When validation reaches cleanup after the successful exit-state inspection.
        let result = fixture
            .validator
            .validate(fixture.prepared.take().unwrap())
            .await;
        // Then no receipt escapes, and ownership remains available for a later cleanup retry.
        assert!(result.is_err());
        assert!(fixture.source.exists());
        assert!(fixture.validator.active.is_some());
        fs::remove_file(fixture.root.join("refuse-cleanup")).unwrap();
        fixture.validator.cleanup().await.unwrap();
        assert!(!fixture.source.exists());
    }

    #[tokio::test]
    async fn silent_process_obeys_the_reader_deadline() {
        // Given a silent process with no output that would otherwise wait for thirty seconds.
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // When the bounded reader's deadline expires without either pipe closing.
        let result = collect(&mut child, 1024, Duration::from_millis(30)).await;
        // Then the reader fails and leaves the child available for its owner's explicit cleanup.
        assert!(result.is_err());
        child.start_kill().unwrap();
        child.wait().await.unwrap();
    }
}
