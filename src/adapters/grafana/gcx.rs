//! Typed construction of the read-only `gcx` query subcommands.
//!
//! This module produces literal argument vectors. It does not invoke a shell,
//! accept arbitrary subcommands, or expose Grafana's generic API surface.

use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::Semaphore,
    time,
};

/// The read-only observability capability selected for a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// Query Loki logs.
    Logs,
    /// Query Prometheus metrics.
    Metrics,
}

/// Classified failures at the `gcx` process boundary.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum GcxRunError {
    /// The query or datasource violated the read-only context policy.
    #[error("gcx query was rejected by policy")]
    InvalidQuery,
    /// The child process could not be started.
    #[error("gcx process could not be started")]
    SpawnFailed,
    /// The child produced more output than the context budget allows.
    #[error("gcx output exceeded its configured limit")]
    OutputLimitExceeded,
    /// Reading the child streams failed.
    #[error("gcx output could not be read")]
    OutputReadFailed,
    /// The child exceeded its wall-clock budget.
    #[error("gcx process timed out")]
    TimedOut,
    /// The child exited unsuccessfully; stderr is intentionally not retained.
    #[error("gcx process exited unsuccessfully with status {0}")]
    NonZeroExit(i32),
    /// The shared GCX concurrency gate was closed unexpectedly.
    #[error("gcx concurrency gate is unavailable")]
    ConcurrencyUnavailable,
}

/// Bounded output returned by a successful `gcx` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcxOutput {
    /// Bytes emitted on stdout by the read-only query.
    pub stdout: Vec<u8>,
}

/// Executes only typed `gcx` queries with process, environment, and output bounds.
#[derive(Debug, Clone)]
pub struct GcxRunner {
    binary: PathBuf,
    timeout: Duration,
    max_output_bytes: usize,
    max_query_bytes: usize,
    concurrency: Arc<Semaphore>,
    global_concurrency: Arc<Semaphore>,
}

const GLOBAL_GCX_CONCURRENCY_LIMIT: usize = 64;
static GLOBAL_GCX_CONCURRENCY: OnceLock<Arc<Semaphore>> = OnceLock::new();

impl GcxRunner {
    /// Creates a runner for a pinned `gcx` binary.
    pub fn new(binary: impl Into<PathBuf>, timeout: Duration, max_output_bytes: usize) -> Self {
        Self {
            binary: binary.into(),
            timeout,
            max_output_bytes,
            max_query_bytes: 4_096,
            concurrency: Arc::new(Semaphore::new(4)),
            global_concurrency: global_concurrency(),
        }
    }

    /// Overrides the maximum UTF-8 expression size accepted by the policy.
    pub fn with_max_query_bytes(mut self, max_query_bytes: usize) -> Self {
        self.max_query_bytes = max_query_bytes;
        self
    }

    /// Sets the maximum number of concurrent GCX child processes.
    pub fn with_max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.concurrency = Arc::new(Semaphore::new(max_concurrency.max(1)));
        self
    }

    /// Executes one typed query without invoking a shell.
    pub async fn run(&self, query: &GcxQuery) -> Result<GcxOutput, GcxRunError> {
        query.validate(self.max_query_bytes)?;
        let _global_permit = self
            .global_concurrency
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| GcxRunError::ConcurrencyUnavailable)?;
        let _permit = self
            .concurrency
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| GcxRunError::ConcurrencyUnavailable)?;
        let mut command = Command::new(&self.binary);
        command
            .args(query.argv())
            .env_clear()
            .env("GCX_NO_UPDATE_NOTIFIER", "1")
            .current_dir("/")
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = command.spawn().map_err(|_| GcxRunError::SpawnFailed)?;
        let stdout = child.stdout.take().ok_or(GcxRunError::OutputReadFailed)?;
        let stderr = child.stderr.take().ok_or(GcxRunError::OutputReadFailed)?;
        let max_output_bytes = self.max_output_bytes;

        time::timeout(self.timeout, async move {
            let stdout_task = tokio::spawn(read_bounded(stdout, max_output_bytes));
            let stderr_task = tokio::spawn(read_bounded(stderr, max_output_bytes));
            let (stdout, stderr) = tokio::try_join!(stdout_task, stderr_task)
                .map_err(|_| GcxRunError::OutputReadFailed)?;
            let stdout = match stdout {
                Ok(value) => value,
                Err(error) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return Err(error);
                }
            };
            if let Err(error) = stderr {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(error);
            }
            let status = child
                .wait()
                .await
                .map_err(|_| GcxRunError::OutputReadFailed)?;

            if !status.success() {
                return Err(GcxRunError::NonZeroExit(status.code().unwrap_or(-1)));
            }

            Ok(GcxOutput { stdout })
        })
        .await
        .map_err(|_| GcxRunError::TimedOut)?
    }
}

fn global_concurrency() -> Arc<Semaphore> {
    GLOBAL_GCX_CONCURRENCY
        .get_or_init(|| Arc::new(Semaphore::new(GLOBAL_GCX_CONCURRENCY_LIMIT)))
        .clone()
}

async fn read_bounded<R>(reader: R, max_output_bytes: usize) -> Result<Vec<u8>, GcxRunError>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take(max_output_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| GcxRunError::OutputReadFailed)?;

    if bytes.len() > max_output_bytes {
        return Err(GcxRunError::OutputLimitExceeded);
    }

    Ok(bytes)
}

/// A provider-neutral request that can be lowered to one fixed `gcx` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcxQuery {
    /// A bounded LogQL query against the configured Loki datasource.
    Logs {
        /// LogQL expression authored by the investigator.
        expression: String,
        /// Fixed datasource UID or alias selected by policy.
        datasource: String,
    },
    /// A bounded PromQL query against the configured Prometheus datasource.
    Metrics {
        /// PromQL expression authored by the investigator.
        expression: String,
        /// Fixed datasource UID or alias selected by policy.
        datasource: String,
    },
}

impl GcxQuery {
    /// Creates a read-only LogQL request.
    pub fn logs(expression: impl Into<String>, datasource: impl Into<String>) -> Self {
        Self::Logs {
            expression: expression.into(),
            datasource: datasource.into(),
        }
    }

    /// Creates a read-only PromQL request.
    pub fn metrics(expression: impl Into<String>, datasource: impl Into<String>) -> Self {
        Self::Metrics {
            expression: expression.into(),
            datasource: datasource.into(),
        }
    }

    /// Returns the capability represented by this query.
    pub const fn kind(&self) -> QueryKind {
        match self {
            Self::Logs { .. } => QueryKind::Logs,
            Self::Metrics { .. } => QueryKind::Metrics,
        }
    }

    /// Lowers the request to a literal argv vector for `gcx`.
    pub fn argv(&self) -> Vec<String> {
        match self {
            Self::Logs {
                expression,
                datasource,
            } => vec![
                "logs".to_owned(),
                "query".to_owned(),
                expression.clone(),
                "-d".to_owned(),
                datasource.clone(),
            ],
            Self::Metrics {
                expression,
                datasource,
            } => vec![
                "metrics".to_owned(),
                "query".to_owned(),
                expression.clone(),
                "-d".to_owned(),
                datasource.clone(),
            ],
        }
    }

    /// Validates model-authored query data before process execution.
    pub fn validate(&self, max_expression_bytes: usize) -> Result<(), GcxRunError> {
        let (expression, datasource, kind) = match self {
            Self::Logs {
                expression,
                datasource,
            }
            | Self::Metrics {
                expression,
                datasource,
            } => (expression, datasource, self.kind()),
        };
        if expression.is_empty()
            || expression.len() > max_expression_bytes
            || datasource.is_empty()
            || datasource.len() > 128
            || !datasource
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            || expression.contains('\0')
            || expression
                .chars()
                .any(|character| matches!(character, '\n' | '\r'))
        {
            return Err(GcxRunError::InvalidQuery);
        }
        let normalized = expression
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        let invalid_semantics = match kind {
            QueryKind::Logs => {
                !normalized.contains('{') || !normalized.contains('}') || normalized.contains("{}")
            }
            QueryKind::Metrics => {
                normalized.contains("{}") || normalized.contains("=~\".*\"") || normalized == "*"
            }
        };
        let prohibited_operations = [
            "--limit",
            "limit0",
            "gcxapi",
            "http://",
            "https://",
            "topk(",
            "bottomk(",
            "count_values(",
            "label_values(",
            "series(",
            "metadata(",
        ];
        if invalid_semantics
            || normalized.contains("=~\".*\"")
            || normalized.contains("=~'.*'")
            || prohibited_operations
                .iter()
                .any(|operation| normalized.contains(operation))
        {
            return Err(GcxRunError::InvalidQuery);
        }
        Ok(())
    }
}
