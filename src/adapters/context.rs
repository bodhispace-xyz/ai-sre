//! Bounded, server-owned context adapters for Git, deployment history, and health.
//!
//! Requests contain only typed selectors. Command paths and argument prefixes
//! are deployment-owned, and every process runs without a shell, inherited
//! environment, or caller working directory.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use thiserror::Error;
use tokio::{io::AsyncReadExt, process::Command, sync::Semaphore, time};

use crate::reasoning::evidence::{
    EvidenceBoard, EvidenceError, EvidenceMetadata, EvidenceSource, EvidenceStatus,
    MAX_EVIDENCE_QUERY_BYTES,
};
use crate::reasoning::tools::{ContextTool, ToolCall, ToolResult, ToolResultClass};

const MAX_SELECTOR_BYTES: usize = 512;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_HISTORY_ENTRIES: usize = 50;
const GLOBAL_CONTEXT_CONCURRENCY_LIMIT: usize = 64;
static GLOBAL_CONTEXT_CONCURRENCY: OnceLock<Arc<Semaphore>> = OnceLock::new();

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
    global_concurrency: Arc<Semaphore>,
}

/// Deployment-owned selectors for the read-only context facade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOnlyContextConfig {
    /// Absolute path to the server-owned Git executable.
    pub git_binary: PathBuf,
    /// Absolute path to the server-owned Git repository.
    pub git_repository: String,
    /// Absolute path to the server-owned health adapter executable.
    pub health_binary: PathBuf,
    /// Health aliases mapped to fixed argument vectors.
    pub health_commands: BTreeMap<String, Vec<String>>,
}

/// Read-only context facade used by orchestration code.
#[derive(Debug, Clone)]
pub struct ReadOnlyContext {
    runner: ReadOnlyRunner,
    config: ReadOnlyContextConfig,
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
            global_concurrency: global_context_concurrency(),
        }
    }

    /// Executes a deployment-owned plan and commits its bounded output.
    pub async fn execute(
        &self,
        board: &mut EvidenceBoard,
        plan: &ReadOnlyCommand,
    ) -> Result<String, ContextError> {
        let _global_permit = self
            .global_concurrency
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ContextError::ConcurrencyUnavailable)?;
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

fn global_context_concurrency() -> Arc<Semaphore> {
    GLOBAL_CONTEXT_CONCURRENCY
        .get_or_init(|| Arc::new(Semaphore::new(GLOBAL_CONTEXT_CONCURRENCY_LIMIT)))
        .clone()
}

impl ReadOnlyContext {
    /// Creates a facade from deployment-owned binaries, repository, and health aliases.
    pub fn new(
        runner: ReadOnlyRunner,
        config: ReadOnlyContextConfig,
    ) -> Result<Self, ContextError> {
        if !config.git_repository.starts_with('/')
            || config.git_repository.contains('\0')
            || config.git_repository.chars().any(char::is_whitespace)
            || config
                .health_commands
                .keys()
                .any(|alias| alias.trim().is_empty())
        {
            return Err(ContextError::InvalidPlan);
        }
        Ok(Self { runner, config })
    }

    /// Reads one bounded desired-state path at a server-selected revision.
    pub async fn desired_state(
        &self,
        board: &mut EvidenceBoard,
        revision: impl Into<String>,
        path: impl Into<String>,
    ) -> Result<String, ContextError> {
        let plan = git_desired_state(
            &self.config.git_binary,
            self.config.git_repository.clone(),
            revision,
            path,
        )?;
        self.runner.execute(board, &plan).await
    }

    /// Reads bounded recent history for one server-selected desired-state path.
    pub async fn deployment_history(
        &self,
        board: &mut EvidenceBoard,
        path: impl Into<String>,
        max_entries: usize,
    ) -> Result<String, ContextError> {
        let plan = deployment_history(
            &self.config.git_binary,
            self.config.git_repository.clone(),
            path,
            max_entries,
        )?;
        self.runner.execute(board, &plan).await
    }

    /// Reads one configured health alias; unknown aliases fail closed.
    pub async fn health(
        &self,
        board: &mut EvidenceBoard,
        alias: &str,
    ) -> Result<String, ContextError> {
        let args = self
            .config
            .health_commands
            .get(alias)
            .cloned()
            .ok_or(ContextError::InvalidPlan)?;
        let plan = health_plan(&self.config.health_binary, args, alias)?;
        self.runner.execute(board, &plan).await
    }

    /// Executes one Git or health tool call using only server-owned selectors.
    pub async fn execute_tool(&self, board: &mut EvidenceBoard, call: &ToolCall) -> ToolResult {
        let source = match call.tool {
            ContextTool::ReadDesiredState => EvidenceSource::GitDesiredState,
            ContextTool::ReadDeploymentHistory => EvidenceSource::DeploymentHistory,
            ContextTool::ReadHealth => EvidenceSource::Health,
            ContextTool::QueryLogs | ContextTool::QueryMetrics => EvidenceSource::Health,
        };
        let result = match call.tool {
            ContextTool::ReadDesiredState => {
                let Some((revision, path)) = call.query.split_once('\n') else {
                    return denied_result(call);
                };
                self.desired_state(board, revision, path).await
            }
            ContextTool::ReadDeploymentHistory => {
                self.deployment_history(board, &call.query, 20).await
            }
            ContextTool::ReadHealth => self.health(board, call.query.trim()).await,
            ContextTool::QueryLogs | ContextTool::QueryMetrics => return denied_result(call),
        };
        let mut result = match result {
            Ok(evidence_id) => ToolResult {
                call_id: call.call_id.clone(),
                class: ToolResultClass::Succeeded,
                evidence_id: Some(evidence_id),
                detail: "bounded evidence committed".to_owned(),
            },
            Err(ContextError::InvalidPlan | ContextError::Evidence(_)) => denied_result(call),
            Err(ContextError::OutputLimitExceeded) => ToolResult {
                call_id: call.call_id.clone(),
                class: ToolResultClass::Truncated,
                evidence_id: None,
                detail: "context output exceeded its bound".to_owned(),
            },
            Err(_) => ToolResult {
                call_id: call.call_id.clone(),
                class: ToolResultClass::Unavailable,
                evidence_id: None,
                detail: "context backend unavailable".to_owned(),
            },
        };
        if result.evidence_id.is_none() {
            let status = match result.class {
                ToolResultClass::Truncated => EvidenceStatus::Partial,
                ToolResultClass::Denied => EvidenceStatus::Rejected,
                ToolResultClass::Exhausted => EvidenceStatus::BudgetExhausted,
                _ => EvidenceStatus::Unavailable,
            };
            result.evidence_id = board
                .commit_with_metadata(
                    source,
                    bounded_failure_query(&call.query),
                    b"[NO EVIDENCE]".to_vec(),
                    EvidenceMetadata {
                        status,
                        freshness_ms: None,
                        truncated: status == EvidenceStatus::Partial,
                        error: Some("read-only context request did not complete".to_owned()),
                    },
                )
                .ok();
        }
        result
    }
}

fn denied_result(call: &ToolCall) -> ToolResult {
    ToolResult {
        call_id: call.call_id.clone(),
        class: ToolResultClass::Denied,
        evidence_id: None,
        detail: "context request rejected by server policy".to_owned(),
    }
}

fn bounded_failure_query(query: &str) -> String {
    if query.len() <= MAX_EVIDENCE_QUERY_BYTES {
        query.to_owned()
    } else {
        "[QUERY_REDACTED]".to_owned()
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
