//! Imports protected deployment evidence through the application-owned journal.
//!
//! Disabled unless deployment supplies an inbox. Importing receipts neither starts validation
//! nor makes a stored candidate ready. A failed scan or journal write stops intake; operators
//! must repair the evidence source rather than bypass a missing revocation or contradiction.

use crate::{
    gitops::{
        inbox::{InboxLimits, ReceiptInbox},
        receipt::ProtectedReceipt,
    },
    reasoning::storage::{JournalStore, JournalStoreError},
};
use std::{
    env,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

/// Safe startup/import errors, without receipt payloads or filesystem paths.
#[derive(Debug, Error)]
pub enum DeploymentIntakeError {
    /// Deployment settings are absent in part or outside resource bounds.
    #[error("invalid deployment inbox configuration")]
    Configuration,
    /// A complete protected scan could not be read.
    #[error("deployment inbox scan failed")]
    Scan,
    /// Durable qualification could not be committed.
    #[error("deployment inbox journal commit failed")]
    Journal(#[from] JournalStoreError),
}

pub(crate) struct DeploymentIntake {
    inbox: ReceiptInbox,
    tick: tokio::time::Interval,
}

impl DeploymentIntake {
    pub(crate) fn from_environment() -> Result<Option<Self>, DeploymentIntakeError> {
        let path = env::var_os("AI_SRE_DEPLOYMENT_INBOX").map(PathBuf::from);
        let entries = env::var_os("AI_SRE_DEPLOYMENT_INBOX_MAX_ENTRIES");
        let bytes = env::var_os("AI_SRE_DEPLOYMENT_INBOX_MAX_BYTES");
        let poll = env::var_os("AI_SRE_DEPLOYMENT_INBOX_POLL_SECONDS");
        Self::configured(path, entries, bytes, poll)
    }

    fn configured(
        path: Option<PathBuf>,
        entries: Option<std::ffi::OsString>,
        bytes: Option<std::ffi::OsString>,
        poll: Option<std::ffi::OsString>,
    ) -> Result<Option<Self>, DeploymentIntakeError> {
        let Some(path) = path else {
            return if entries.is_some() || bytes.is_some() || poll.is_some() {
                Err(DeploymentIntakeError::Configuration)
            } else {
                Ok(None)
            };
        };
        let parse = |value: Option<std::ffi::OsString>,
                     default: u64|
         -> Result<u64, DeploymentIntakeError> {
            value
                .map(|value| {
                    value
                        .to_str()
                        .and_then(|value| value.parse().ok())
                        .ok_or(DeploymentIntakeError::Configuration)
                })
                .unwrap_or(Ok(default))
        };
        let limits = InboxLimits {
            entries: usize::try_from(parse(entries, 256)?)
                .map_err(|_| DeploymentIntakeError::Configuration)?,
            bytes: parse(bytes, 16 * 1024 * 1024)?,
        };
        let seconds = parse(poll, 15)?;
        if !(1..=3600).contains(&seconds) {
            return Err(DeploymentIntakeError::Configuration);
        }
        let inbox =
            ReceiptInbox::new(path, limits).map_err(|_| DeploymentIntakeError::Configuration)?;
        let period = Duration::from_secs(seconds);
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Ok(Some(Self { inbox, tick }))
    }

    async fn refresh(&self, journal: &mut JournalStore) -> Result<(), DeploymentIntakeError> {
        let inbox = self.inbox.clone();
        let receipts = read_off_thread(move || inbox.read()).await?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| DeploymentIntakeError::Scan)?
            .as_secs();
        record(journal, &receipts, now).await
    }
}

pub(crate) async fn next(intake: &mut Option<DeploymentIntake>) {
    match intake {
        Some(intake) => {
            intake.tick.tick().await;
        }
        None => std::future::pending().await,
    }
}

pub(crate) async fn refresh(
    intake: &Option<DeploymentIntake>,
    journal: &mut JournalStore,
) -> Result<(), DeploymentIntakeError> {
    if let Some(intake) = intake {
        intake.refresh(journal).await?;
    }
    Ok(())
}

// Each receipt and its audit fact commit atomically. The single journal owner must stop on error;
// no repair may run between these commits or after a partial failed import.
async fn record(
    journal: &mut JournalStore,
    receipts: &[ProtectedReceipt],
    now: u64,
) -> Result<(), DeploymentIntakeError> {
    for receipt in receipts {
        journal.record_deployment(receipt, now)?;
        // Keep journal ownership here, but let HTTP handling and timers run between commits.
        tokio::task::yield_now().await;
    }
    Ok(())
}

// There is at most one scan in flight: refresh awaits this job before accepting another scan.
// Cancellation does not cancel blocking filesystem I/O; the local-storage contract still applies.
async fn read_off_thread(
    read: impl FnOnce() -> Result<Vec<ProtectedReceipt>, crate::gitops::receipt::ReceiptError>
    + Send
    + 'static,
) -> Result<Vec<ProtectedReceipt>, DeploymentIntakeError> {
    tokio::task::spawn_blocking(read)
        .await
        .map_err(|_| DeploymentIntakeError::Scan)?
        .map_err(|_| DeploymentIntakeError::Scan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn filesystem_errors_and_worker_panics_reject_the_scan() {
        // Given a filesystem error or a blocking worker that fails before returning evidence.
        let failed = read_off_thread(|| Err(crate::gitops::receipt::ReceiptError::Unprotected));
        let panicked = read_off_thread(|| panic!("simulated scan failure"));
        // When either failure crosses the blocking-worker boundary.
        let results = [failed.await, panicked.await];
        // Then neither failure becomes an empty successful scan that could hide a revocation.
        for result in results {
            assert!(matches!(result, Err(DeploymentIntakeError::Scan)));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_filesystem_read_does_not_block_runtime_timers() {
        // Given a scan that cannot finish until the runtime responds to its start signal.
        let (started, waiting) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let scan = tokio::spawn(read_off_thread(move || {
            started.send(()).unwrap();
            blocked
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| crate::gitops::receipt::ReceiptError::Unprotected)?;
            Ok(Vec::new())
        }));
        waiting.await.unwrap();
        // When a timer runs on the same single-thread runtime while filesystem work remains blocked.
        tokio::time::sleep(Duration::from_millis(10)).await;
        release.send(()).unwrap();
        // Then the scan completes only after that runtime task releases it, without a deadlock.
        assert!(scan.await.unwrap().unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn journal_import_yields_to_other_tasks_between_receipts() {
        use std::sync::atomic::{AtomicBool, Ordering};
        // Given a ready import and another task on the same runtime thread.
        let mut journal = JournalStore::open(":memory:").unwrap();
        let receipts = [
            crate::gitops::receipt::fixture(),
            crate::gitops::receipt::fixture(),
        ];
        let completed = AtomicBool::new(false);
        // When journal imports run, another ready task must run before the whole batch finishes.
        let (_, ran_during_import) = tokio::join!(
            async {
                record(&mut journal, &receipts, 221).await.unwrap();
                completed.store(true, Ordering::SeqCst);
            },
            async { !completed.load(Ordering::SeqCst) }
        );
        // Then the journal remains single-owner and replay still creates only one audit fact.
        assert!(ran_during_import);
        assert_eq!(journal.journal().entries().len(), 1);
    }

    #[tokio::test]
    async fn deployment_settings_are_opt_in_and_invalid_limits_never_fall_back() {
        // Given no deployment settings, or partial and out-of-range operator settings.
        assert!(
            DeploymentIntake::configured(None, None, None, None)
                .unwrap()
                .is_none()
        );
        assert!(DeploymentIntake::configured(None, Some("1".into()), None, None).is_err());
        // When an inbox is explicitly configured, each supplied limit must be valid.
        let path = || Some(PathBuf::from("/var/lib/ai-sre/receipts"));
        for value in ["0", "4097", "invalid"] {
            assert!(DeploymentIntake::configured(path(), Some(value.into()), None, None).is_err());
        }
        for value in ["0", "67108865", "invalid"] {
            assert!(DeploymentIntake::configured(path(), None, Some(value.into()), None).is_err());
        }
        for value in ["0", "3601", "invalid"] {
            assert!(DeploymentIntake::configured(path(), None, None, Some(value.into())).is_err());
        }
        // Then only valid explicit configuration creates an importer; it still grants no repair action.
        assert!(
            DeploymentIntake::configured(path(), None, None, None)
                .unwrap()
                .is_some()
        );
        assert!(DeploymentIntake::configured(Some("relative".into()), None, None, None).is_err());
    }

    #[tokio::test]
    async fn complete_scan_revocation_wins_and_replay_never_requalifies_it() {
        // Given an accepted deployment and a later protected revocation of that identity.
        let mut journal = JournalStore::open(":memory:").unwrap();
        let receipt = crate::gitops::receipt::fixture();
        record(&mut journal, &[receipt], 221).await.unwrap();
        let mut revoked = crate::gitops::receipt::fixture();
        revoked.wire.revoked = true;
        let id = revoked.deployment_id().to_owned();
        // When one scan includes both original publication and revocation, in either name order.
        record(
            &mut journal,
            &[revoked, crate::gitops::receipt::fixture()],
            222,
        )
        .await
        .unwrap();
        let events = journal.journal().entries().len();
        record(&mut journal, &[crate::gitops::receipt::fixture()], 223)
            .await
            .unwrap();
        // Then old-file replay cannot undo the revocation or add duplicate audit facts.
        assert!(journal.qualified_deployment(&id, 223).unwrap().is_none());
        assert_eq!(journal.journal().entries().len(), events);
    }

    #[tokio::test]
    async fn repeated_scans_preserve_audit_identity_but_recheck_health_age() {
        // Given a complete valid receipt already committed by the application owner.
        let mut journal = JournalStore::open(":memory:").unwrap();
        let receipt = crate::gitops::receipt::fixture();
        let id = receipt.deployment_id().to_owned();
        record(&mut journal, &[receipt], 221).await.unwrap();
        // When an identical publication is read again, and later becomes stale.
        record(&mut journal, &[crate::gitops::receipt::fixture()], 222)
            .await
            .unwrap();
        assert!(journal.qualified_deployment(&id, 222).unwrap().is_some());
        record(&mut journal, &[crate::gitops::receipt::fixture()], 900)
            .await
            .unwrap();
        // Then polling does not grow the journal or keep expired qualification alive.
        assert_eq!(journal.journal().entries().len(), 1);
        assert!(journal.qualified_deployment(&id, 900).unwrap().is_none());
    }
}
