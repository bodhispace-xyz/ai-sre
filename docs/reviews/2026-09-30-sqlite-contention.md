# SQLite contention diagnostic

## Method

Run the isolated diagnostic explicitly:

```sh
nix develop --command cargo test --test runtime_contention -- --ignored --nocapture
```

It uses a disposable SQLite database with the real `JournalStore`, WAL and FULL
synchronous settings. Another connection holds an immediate writer transaction
for 400 ms. A separate OS thread makes 24 sequential authenticated HTTP requests
to the real metrics endpoint, with 20 ms between responses. A timer shares the
single-thread Tokio executor with the server.

The first scenario appends inline, as the application does today. The second
moves the whole journal owner into one `spawn_blocking` job for the experiment.
Both verify the durable event after reopening the database. Production ownership
and transactions are unchanged.

## Observed results

Two local macOS runs on 2026-09-30 produced these values, in milliseconds:

| Run | Mode | HTTP median | HTTP maximum | Timer delay | Journal write |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Inline | 1.229 | 455.283 | 444.326 | 453.154 |
| 1 | Off-thread | 1.418 | 3.566 | 1.222 | 452.150 |
| 2 | Inline | 0.913 | 468.252 | 457.163 | 466.911 |
| 2 | Off-thread | 0.886 | 1.200 | 1.304 | 453.662 |

The second run overlapped local CI, so this is not a controlled benchmark.
The repeatable observation is executor starvation during synchronous SQLite
contention, not a production throughput or percentile claim. Storage still waits
when off-thread; moving it does not make the transaction faster.

Only one request in each inline sequence hit the long stall. A 24-sample P95
therefore hides the worst request. Inspect maxima and request distributions
rather than treating a low median as proof of responsiveness. The printed P50
and P95 use nearest-rank sample positions, not histogram estimates.

## Limits and next decision

This is a metrics endpoint diagnostic, not a full incident replay or a test of
production disk latency. HTTP time includes connect, write, and response read.
The fixture has one contending writer and one journal event. It does not prove
all storage paths, crash recovery, or shutdown under a permanently stalled disk.
The diagnostic is ignored in normal CI because it deliberately induces timing
effects; CI still compiles and lints it.

The result supports isolating synchronous journal work from HTTP scheduling.
Before implementation, review a single-owner storage-thread boundary with a
bounded command queue. Commands must retain existing transaction order and
return acknowledgements only after durable completion. Dropping a response
future must not mean a submitted write was cancelled. Shutdown must explicitly
drain or reject queued work and account for any write already running.

Do not replace the store with `Arc<Mutex<JournalStore>>`, scatter independent
blocking calls across the pool, or change SQLite durability settings. The
experiment's temporary ownership transfer is evidence, not the final application
architecture. A storage-thread refactor needs deeper review of all journal users,
manual-repair qualification, reservation ordering, and restart semantics.

## Implemented isolation boundary

Caller review found the journal tightly coupled to the single incident owner.
This branch therefore implements [incident-owner isolation](../engineering/incident-owner-isolation.md),
not independent per-method storage commands. It retains one mutable journal and
the existing bounded intake queue. The real service-binary regression confirms
HTTP progress during actual SQLite contention and acknowledgement only after
writer release. Review the documented cancellation and shutdown limits before merge.

Verification on 2026-09-30:

- Full local CI passed: formatting, strict Clippy, 238 tests, documentation,
  and dependency checks. Two opt-in Tempo tests and the induced-contention
  diagnostic were skipped by the ordinary suite; the diagnostic passed twice
  when run explicitly as recorded above.
- The actual service-binary contention test and both owner-boundary tests passed
  explicitly on isolated Linux as well as in the local suite.
- The existing Linux-root dispatch reservation/restart test and protected inbox
  revocation test passed explicitly with synthetic fixtures.
- No production deployment, credentials, or enrollment changed.

Prioritized review handoff:

1. Must review before merge: owner cancellation and completion lifetime,
   startup readiness, durable acknowledgement order, and fail-stop handling
   when shutdown cannot confirm journal release.
2. Safe to defer: worker-local timer isolation, production workload benchmarks,
   and a database-only async facade. Graceful OS signal handling remains
   unproven and must not be claimed by this batch.
3. Deliberate decisions: move the coupled incident owner instead of individual
   storage calls; retain the existing bounded queue and durability settings;
   use process-level recovery after unconfirmed shutdown rather than replacing
   an owner in place.
