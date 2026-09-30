//! Isolates the single incident and journal owner from the HTTP executor.
//!
//! Shutdown cancels asynchronous work but cannot interrupt a synchronous SQLite call.
//! Completion is reported only after the owner future and its journal have been dropped.

use std::{future::Future, io, time::Duration};
use tokio::sync::{oneshot, watch};

pub(super) struct OwnerThread<T> {
    stop: watch::Sender<bool>,
    finished: oneshot::Receiver<io::Result<Option<T>>>,
}

impl<T: Send + 'static> OwnerThread<T> {
    pub(super) fn spawn<F, Fut>(factory: F) -> io::Result<Self>
    where
        F: FnOnce(watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
    {
        let (stop, mut stopping) = watch::channel(false);
        let (finished, receiver) = oneshot::channel();
        std::thread::Builder::new()
            .name("ai-sre-owner".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = finished.send(Err(error));
                        return;
                    }
                };
                let result = runtime.block_on(async move {
                    let work = factory(stopping.clone());
                    tokio::pin!(work);
                    tokio::select! {
                        biased;
                        _ = stopping.changed() => None,
                        output = &mut work => Some(output),
                    }
                });
                // Blocking filesystem scans may outlive runtime shutdown; none owns the journal.
                runtime.shutdown_timeout(Duration::from_millis(100));
                let _ = finished.send(Ok(result));
            })?;
        Ok(Self {
            stop,
            finished: receiver,
        })
    }

    pub(super) async fn wait(&mut self) -> io::Result<Option<T>> {
        (&mut self.finished)
            .await
            .map_err(|_| io::Error::other("incident owner failed"))?
    }

    pub(super) async fn shutdown(&mut self) -> io::Result<Option<T>> {
        self.shutdown_with_timeout(Duration::from_secs(10)).await
    }

    async fn shutdown_with_timeout(&mut self, deadline: Duration) -> io::Result<Option<T>> {
        let _ = self.stop.send(true);
        tokio::time::timeout(deadline, self.wait())
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "incident owner shutdown is unconfirmed",
                )
            })?
    }
}

impl<T> Drop for OwnerThread<T> {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        observability::MetricsSnapshot,
        reasoning::{journal::JournalEvent, storage::JournalStore},
        transport::{AlertIntake, IntakeCommand, IntakeConfig},
    };

    fn event() -> JournalEvent {
        JournalEvent::IncidentOpened {
            incident_id: "synthetic-owner".into(),
            alert_name: "Synthetic".into(),
            labels: Default::default(),
            annotations: Default::default(),
            event_time: String::new(),
            source_event_id: String::new(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn owner_keeps_http_responsive_and_commits_before_shutdown_acknowledgement() {
        // Given the real owner boundary, a one-entry queue, and an actual SQLite writer lock.
        let path = std::env::temp_dir().join(format!("ai-sre-owner-{}.sqlite", std::process::id()));
        assert!(!path.exists());
        drop(JournalStore::open(&path).unwrap());
        let (ready, initialized) = oneshot::channel();
        let (commands, mut requests) = tokio::sync::mpsc::channel::<oneshot::Sender<()>>(1);
        let (started, writing) = oneshot::channel();
        let owner_path = path.clone();
        let mut owner = OwnerThread::spawn(move |stopping| async move {
            let mut journal = JournalStore::open(owner_path).unwrap();
            ready.send(()).unwrap();
            let mut started = Some(started);
            while let Some(reply) = requests.recv().await {
                if *stopping.borrow() {
                    break;
                }
                if let Some(started) = started.take() {
                    started.send(()).unwrap();
                }
                journal.append(event()).unwrap();
                let _ = reply.send(());
                if *stopping.borrow() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .unwrap();
        initialized.await.unwrap();
        let (locked, lock_ready) = oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let lock_path = path.clone();
        let writer = std::thread::spawn(move || {
            let database = rusqlite::Connection::open(lock_path).unwrap();
            database.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked.send(()).unwrap();
            held.recv_timeout(Duration::from_secs(5)).unwrap();
            database.execute_batch("COMMIT").unwrap();
        });
        lock_ready.await.unwrap();
        let (ack, mut committed) = oneshot::channel();
        commands.send(ack).await.unwrap();
        writing.await.unwrap();
        let (pending, rejected) = oneshot::channel();
        commands.send(pending).await.unwrap();
        let (overflow, _) = oneshot::channel();
        assert!(matches!(
            commands.try_send(overflow),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_))
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, _receiver) = tokio::sync::mpsc::channel::<IntakeCommand>(1);
        let server = tokio::spawn(
            AlertIntake::new(IntakeConfig::default())
                .with_bearer_tokens("synthetic-owner-token", Option::<String>::None)
                .serve_with_metrics(listener, sender, MetricsSnapshot::default()),
        );
        // When SQLite is still blocked, HTTP and timers progress independently.
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            reqwest::Client::new()
                .get(format!("http://{address}/metrics"))
                .bearer_auth("synthetic-owner-token")
                .send(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(matches!(
            committed.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        owner.stop.send(true).unwrap();
        // Then shutdown does not fabricate a commit or cancel the running transaction.
        assert_eq!(
            owner
                .shutdown_with_timeout(Duration::from_millis(20))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        release.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), committed)
                .await
                .unwrap()
                .is_ok()
        );
        owner.shutdown().await.unwrap();
        assert!(rejected.await.is_err());
        assert!(commands.send(oneshot::channel().0).await.is_err());
        writer.join().unwrap();
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        let reopened = JournalStore::open(&path).unwrap();
        assert_eq!(reopened.journal().entries().len(), 1);
        drop(reopened);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn panic_or_cancellation_never_reports_successful_owner_completion() {
        // Given an owner that panics and another awaiting work forever.
        let mut failed =
            OwnerThread::<()>::spawn(|_| async { panic!("synthetic owner failure") }).unwrap();
        let mut stopped = OwnerThread::<()>::spawn(|_| std::future::pending()).unwrap();
        // When supervision observes failure or requests shutdown.
        assert!(failed.wait().await.is_err());
        // Then cancellation is distinct from a normally completed result.
        assert!(stopped.shutdown().await.unwrap().is_none());
    }
}
