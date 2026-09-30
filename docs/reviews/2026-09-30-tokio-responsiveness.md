# Tokio responsiveness measurements

This batch adds measurements before changing journal ownership or runtime
architecture. It applies the measurement-first approach from
[Principles for fast Tokio applications](https://dial9-rs.github.io/blog/principles-for-fast-tokio-applications/).

## Measurements

The authenticated metrics endpoint exports three fixed operation groups:

- `ai_sre_runtime_webhook_*`: handler time, including authentication, parsing,
  queue admission, and durable acknowledgement wait. It excludes body download
  before the handler and response transmission. It includes handler errors and
  cancellation; it is not successful-webhook latency alone.
- `ai_sre_runtime_receipt_journal_*`: one synchronous `record_deployment` call,
  including replay, qualification work, transaction waits, and failures. Other
  journal methods are not measured by this group.
- `ai_sre_runtime_timer_delay_*`: delay beyond the scheduled one-second
  heartbeat. Missed ticks are skipped. This is application timer delay, not
  Tokio scheduler latency or a diagnosis of its cause.

Each group has `observations_total`, `microseconds_total`, and
`microseconds_max`. The maximum is the process-lifetime maximum, not a windowed
percentile. These aggregates cannot calculate P99. Atomic reads are approximate
under concurrent updates, not a transactional snapshot.

`ai_sre_runtime_intake_queue_depth_max` is the largest sampled occupancy before
and after webhook enqueue. Reserved permits count as occupied capacity. It is
not current depth and can miss short-lived peaks between observations.

All values reset on process restart. They have no incident labels and do not
write journal facts. Duration measurements use monotonic time. The exporter
renders the journal snapshot before acquiring its write lock.

## Regression coverage

The current-thread tests check that held off-thread work leaves timers,
metrics-like reads, and probe cancellation available. A separate intentional
inline stall verifies that the heartbeat detects timer delay. Existing receipt
intake tests exercise the real scan boundary and yielding between commits.
The authenticated metrics HTTP contract also runs on a current-thread runtime
while off-thread work is held. It requires an HTTP response within two seconds
and checks server cancellation before releasing that work.

These tests do not prove real HTTP tail latency under slow SQLite, or cancel
already-running blocking I/O. Do not treat an async timeout as a guarantee that
a blocking operation stopped.

## Next evidence gate

Replay incident bursts in isolated staging while scraping these metrics. Compare
queue pressure, receipt operation time, and timer delay. Add end-to-end HTTP
latency distributions and a controlled SQLite-contention scenario before making
a journal-thread decision. Preserve transaction order, audit durability, and
explicit shutdown semantics if that decision requires a bounded command channel.

Multiple runtimes, CPU pinning, and spinning remain deferred. No production
performance improvement is claimed by this measurement-only batch.
