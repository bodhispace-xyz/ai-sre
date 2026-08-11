//! Bounded, server-owned context adapters for Git, deployment history, and health.
//!
//! Requests contain only typed selectors. Command paths and argument prefixes
//! are deployment-owned, and every process runs without a shell, inherited
//! environment, or caller working directory.

use std::{path::PathBuf, sync::Arc, time::Duration};

use thiserror::Error;
use tokio::{io::AsyncReadExt, process::Command, sync::Semaphore, time};

use crate::reasoning::evidence::{EvidenceBoard, EvidenceError, EvidenceSource};

const MAX_SELECTOR_BYTES: usize = 512;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_HISTORY_ENTRIES: usize = 50;

/// A fixed, read-only capability that may be exposed to an investigator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextCapability {
    /// Read the desired-state file for one canonical service.
    GitDesiredState,
    /// Read recent deployment history for one canonical service.
    DeploymentHistory,
    /// Read health from one configured endpoint alias.
    Health,
}

/// Server-owned process plan. It cannot be constructed from model text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnlyCommand {
    binary: PathBuf,
    args: Vec<String>,
    source: EvidenceSource,
    query: String,
}

impl ReadOnlyCommand {
    /// Creates a deployment-owned command plan after validating fixed fields.
    pub fn new(
        binary: impl Into<PathBuf>,
        args: Vec<String>,
        source: EvidenceSource,
        query: impl Into<String>,
    ) -> Result<Self, ContextError> {
        let binary = binary.into();
        let query = query.into();
        if !binary.is_absolute() || query.trim().is_empty() || query.len() > MAX_SELECTOR_BYTES {
            return Err(ContextError::InvalidPlan);
        }
        if args.iter().any(|arg| arg.len() > MAX_SELECTOR_BYTES) {
            return Err(ContextError::InvalidPlan);
        }
        Ok(Self {
            binary,
            args,
            source,
            query,
        })
    }
}

/// Bounded process runner shared by non-Grafana read-only context adapters.
#[derive(Debug, Clone)]
pub struct ReadOnlyRunner {
    timeout: Duration,
    max_output_bytes: usize,
    concurrency: Arc<Semaphore>,
}

/// Safe failures at the read-only process boundary.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum ContextError {
    /// The deployment-owned command plan is invalid.
    #[error("read-only command plan is invalid")]
    InvalidPlan,
    /// The process could not be started.
    #[error("read-only context process could not be started")]
    SpawnFailed,
    /// The process exceeded its output budget.
    #[error("read-only context output exceeded its bound")]
    OutputLimitExceeded,
    /// The process exceeded its wall-clock budget.
    #[error("read-only context process timed out")]
    TimedOut,
    /// The process exited unsuccessfully.
    #[error("read-only context process exited unsuccessfully")]
    NonZeroExit,
    /// Evidence could not be committed.
    #[error("read-only context evidence could not be committed")]
    Evidence(#[from] EvidenceError),
    /// The concurrency gate was closed.
    #[error("read-only context concurrency gate is unavailable")]
    ConcurrencyUnavailable,
}

impl ReadOnlyRunner {
    /// Creates a runner with bounded process duration, output, and concurrency.
    pub fn new(timeout: Duration, max_output_bytes: usize, max_concurrency: usize) -> Self {
        Self {
            timeout,
            max_output_bytes: max_output_bytes.clamp(1, MAX_OUTPUT_BYTES),
            concurrency: Arc::new(Semaphore::new(max_concurrency.max(1))),
        }
    }

    /// Executes a deployment-owned plan and commits its bounded output.
    pub async fn execute(
        &self,
        board: &mut EvidenceBoard,
        plan: &ReadOnlyCommand,
    ) -> Result<String, ContextError> {
        let _permit = self
            .concurrency
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ContextError::ConcurrencyUnavailable)?;
        let mut child = Command::new(&plan.binary)
            .args(&plan.args)
            .env_clear()
            .current_dir("/")
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|_| ContextError::SpawnFailed)?;
        let stdout = child.stdout.take().ok_or(ContextError::SpawnFailed)?;
        let limit = self.max_output_bytes;
        time::timeout(self.timeout, async {
            let mut bytes = Vec::new();
            stdout
                .take(limit.saturating_add(1) as u64)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| ContextError::SpawnFailed)?;
            if bytes.len() > limit {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(ContextError::OutputLimitExceeded);
            }
            let status = child.wait().await.map_err(|_| ContextError::SpawnFailed)?;
            if !status.success() {
                return Err(ContextError::NonZeroExit);
            }
            board
                .commit(plan.source, plan.query.clone(), bytes)
                .map_err(ContextError::Evidence)
        })
        .await
        .map_err(|_| ContextError::TimedOut)?
    }
}

/// Fixed Git command plan for a canonical desired-state file.
pub fn git_desired_state(
    binary: impl Into<PathBuf>,
    repository: impl Into<String>,
    revision: impl Into<String>,
    path: impl Into<String>,
) -> Result<ReadOnlyCommand, ContextError> {
    let repository = bounded_selector(repository.into())?;
    let revision = git_revision(revision.into())?;
    let path = relative_git_path(path.into())?;
    ReadOnlyCommand::new(
        binary,
        vec![
            "--no-pager".into(),
            "-C".into(),
            repository.clone(),
            "show".into(),
            "--no-ext-diff".into(),
            "--format=".into(),
            revision.clone(),
            "--".into(),
            path.clone(),
        ],
        EvidenceSource::GitDesiredState,
        format!("git desired state {revision}:{path}"),
    )
}

/// Fixed Git command plan for bounded recent deployment history.
pub fn deployment_history(
    binary: impl Into<PathBuf>,
    repository: impl Into<String>,
    path: impl Into<String>,
    max_entries: usize,
) -> Result<ReadOnlyCommand, ContextError> {
    let repository = bounded_selector(repository.into())?;
    let path = relative_git_path(path.into())?;
    let max_entries = max_entries.clamp(1, MAX_HISTORY_ENTRIES);
    ReadOnlyCommand::new(
        binary,
        vec![
            "--no-pager".into(),
            "-C".into(),
            repository,
            "log".into(),
            "--no-ext-diff".into(),
            "--format=%H %cI %s".into(),
            format!("-n{max_entries}"),
            "--".into(),
            path.clone(),
        ],
        EvidenceSource::DeploymentHistory,
        format!("deployment history {path}"),
    )
}

/// Creates a health plan from a server-owned binary and fixed argument list.
pub fn health_plan(
    binary: impl Into<PathBuf>,
    args: Vec<String>,
    endpoint_alias: impl Into<String>,
) -> Result<ReadOnlyCommand, ContextError> {
    let alias = bounded_selector(endpoint_alias.into())?;
    ReadOnlyCommand::new(
        binary,
        args,
        EvidenceSource::Health,
        format!("health {alias}"),
    )
}

fn bounded_selector(value: String) -> Result<String, ContextError> {
    if value.trim().is_empty()
        || value.len() > MAX_SELECTOR_BYTES
        || value.contains('\0')
        || value.chars().any(char::is_whitespace)
    {
        return Err(ContextError::InvalidPlan);
    }
    Ok(value)
}

fn git_revision(value: String) -> Result<String, ContextError> {
    let value = bounded_selector(value)?;
    if value.starts_with('-') {
        return Err(ContextError::InvalidPlan);
    }
    Ok(value)
}

fn relative_git_path(value: String) -> Result<String, ContextError> {
    let value = bounded_selector(value)?;
    let path = std::path::Path::new(&value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(ContextError::InvalidPlan);
    }
    Ok(value)
}
