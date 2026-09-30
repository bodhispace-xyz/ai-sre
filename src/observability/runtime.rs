//! Measures process-local responsiveness without payloads, identifiers, or journal writes.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Fixed operation names prevent user-controlled metric labels.
#[derive(Clone, Copy)]
pub enum Operation {
    /// Complete webhook handling, including durable acknowledgement wait.
    Webhook,
    /// One synchronous deployment-receipt journal operation, including errors and replay.
    ReceiptJournal,
    /// Delay beyond a scheduled runtime heartbeat.
    TimerDelay,
}

#[derive(Default)]
struct Timing {
    count: AtomicU64,
    micros: AtomicU64,
    maximum: AtomicU64,
}

/// Restart-reset diagnostic measurements, independent of durable incident facts.
#[derive(Clone, Default)]
pub struct RuntimeMetrics(Arc<[Timing; 3]>, Arc<AtomicU64>);

impl RuntimeMetrics {
    /// Records the largest observed intake occupancy, including reserved channel permits.
    /// This is a sampled high-water mark, not a current queue-depth gauge.
    pub fn observe_queue_depth(&self, depth: usize) {
        self.1
            .fetch_max(u64::try_from(depth).unwrap_or(u64::MAX), Ordering::Relaxed);
    }
    /// Starts a duration measurement that records on completion, error, or cancellation.
    pub fn measure(&self, operation: Operation) -> Measurement {
        Measurement {
            metrics: self.clone(),
            operation,
            started: Instant::now(),
        }
    }

    /// Records a monotonic duration with fixed memory use and no locks.
    pub fn observe(&self, operation: Operation, duration: Duration) {
        let value = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        let timing = &self.0[operation as usize];
        timing.count.fetch_add(1, Ordering::Relaxed);
        timing.micros.fetch_add(value, Ordering::Relaxed);
        timing.maximum.fetch_max(value, Ordering::Relaxed);
    }

    pub(super) fn render(&self) -> String {
        let mut output = format!(
            "ai_sre_runtime_intake_queue_depth_max {}\n",
            self.1.load(Ordering::Relaxed)
        );
        for (index, name) in ["webhook", "receipt_journal", "timer_delay"]
            .iter()
            .enumerate()
        {
            let timing = &self.0[index];
            for (suffix, value) in [
                ("observations_total", timing.count.load(Ordering::Relaxed)),
                ("microseconds_total", timing.micros.load(Ordering::Relaxed)),
                ("microseconds_max", timing.maximum.load(Ordering::Relaxed)),
            ] {
                output.push_str(&format!("ai_sre_runtime_{name}_{suffix} {value}\n"));
            }
        }
        output
    }

    /// Runs a one-second heartbeat; dropping the future stops it during shutdown.
    /// Missed ticks are skipped, so stalls cannot cause a burst of catch-up observations.
    pub async fn heartbeat(&self) {
        self.heartbeat_every(Duration::from_secs(1)).await;
    }

    async fn heartbeat_every(&self, period: Duration) {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await;
        self.observe_ticks(tick).await;
    }

    async fn observe_ticks(&self, mut tick: tokio::time::Interval) {
        loop {
            let expected = tick.tick().await;
            self.observe(
                Operation::TimerDelay,
                tokio::time::Instant::now().saturating_duration_since(expected),
            );
        }
    }
}

/// Records elapsed time when dropped; no lock or asynchronous cleanup is needed.
pub struct Measurement {
    metrics: RuntimeMetrics,
    operation: Operation,
    started: Instant,
}

impl Drop for Measurement {
    fn drop(&mut self) {
        self.metrics.observe(self.operation, self.started.elapsed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn inline_blocking_work_is_visible_as_timer_delay() {
        // Given a running heartbeat on the same single-thread executor as application work.
        let metrics = RuntimeMetrics::default();
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await;
        let probe = tokio::spawn({
            let metrics = metrics.clone();
            async move { metrics.observe_ticks(tick).await }
        });
        tokio::task::yield_now().await;
        // When a storage-like synchronous operation holds the executor past a timer deadline.
        std::thread::sleep(Duration::from_millis(60));
        tokio::time::timeout(Duration::from_secs(1), async {
            while metrics.0[Operation::TimerDelay as usize]
                .count
                .load(Ordering::Relaxed)
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Then the probe reports the stall; this does not claim the blocking operation is fixed.
        assert!(
            metrics.0[Operation::TimerDelay as usize]
                .maximum
                .load(Ordering::Relaxed)
                >= 30_000
        );
        probe.abort();
        assert!(probe.await.unwrap_err().is_cancelled());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_blocking_work_leaves_runtime_progress_and_shutdown_available() {
        // Given off-thread storage-like work held until this runtime releases it.
        let (release, wait) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let blocking = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        ready.await.unwrap();
        let metrics = RuntimeMetrics::default();
        let probe = tokio::spawn({
            let metrics = metrics.clone();
            async move { metrics.heartbeat().await }
        });
        // When timers and metrics-like reads execute while storage is still blocked.
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            assert!(metrics.render().contains("timer_delay_observations_total"));
            probe.abort();
            assert!(probe.await.unwrap_err().is_cancelled());
        })
        .await
        .unwrap();
        // Then runtime shutdown can cancel the probe; blocking work needs its own release.
        release.send(()).unwrap();
        blocking.await.unwrap();
    }

    #[test]
    fn diagnostics_have_fixed_names_and_do_not_replace_durable_metrics() {
        // Given two process-local observations of one fixed operation.
        let metrics = RuntimeMetrics::default();
        // When observations have different durations.
        metrics.observe(Operation::ReceiptJournal, Duration::from_micros(20));
        metrics.observe(Operation::ReceiptJournal, Duration::from_micros(50));
        metrics.observe_queue_depth(4);
        metrics.observe_queue_depth(2);
        // Then count, total, and maximum remain separate and restart resets them.
        let text = metrics.render();
        assert!(text.contains("intake_queue_depth_max 4"));
        assert!(text.contains("receipt_journal_observations_total 2"));
        assert!(text.contains("receipt_journal_microseconds_total 70"));
        assert!(text.contains("receipt_journal_microseconds_max 50"));
        assert!(
            RuntimeMetrics::default()
                .render()
                .contains("receipt_journal_observations_total 0")
        );
    }
}
