//! Typed construction of the read-only `gcx` query subcommands.
//!
//! This module produces literal argument vectors. It does not invoke a shell,
//! accept arbitrary subcommands, or expose Grafana's generic API surface.

use std::{path::PathBuf, time::Duration};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
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
}

impl GcxRunner {
    /// Creates a runner for a pinned `gcx` binary.
    pub fn new(binary: impl Into<PathBuf>, timeout: Duration, max_output_bytes: usize) -> Self {
        Self {
            binary: binary.into(),
            timeout,
            max_output_bytes,
        }
    }

    /// Executes one typed query without invoking a shell.
    pub async fn run(&self, query: &GcxQuery) -> Result<GcxOutput, GcxRunError> {
        let mut command = Command::new(&self.binary);
        command
            .args(query.argv())
            .env_clear()
            .env("GCX_NO_UPDATE_NOTIFIER", "1")
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = command.spawn().map_err(|_| GcxRunError::SpawnFailed)?;
        let stdout = child.stdout.take().ok_or(GcxRunError::OutputReadFailed)?;
        let stderr = child.stderr.take().ok_or(GcxRunError::OutputReadFailed)?;
        let max_output_bytes = self.max_output_bytes;

        time::timeout(self.timeout, async move {
            let (stdout, stderr) = tokio::join!(
                read_bounded(stdout, max_output_bytes),
                read_bounded(stderr, max_output_bytes)
            );

            let status = child
                .wait()
                .await
                .map_err(|_| GcxRunError::OutputReadFailed)?;
            let stdout = stdout?;
            stderr?;

            if !status.success() {
                return Err(GcxRunError::NonZeroExit(status.code().unwrap_or(-1)));
            }

            Ok(GcxOutput { stdout })
        })
        .await
        .map_err(|_| GcxRunError::TimedOut)?
    }
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
}
