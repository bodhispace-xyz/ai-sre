# Receipt delivery acceptance deadlines

The Python-to-Rust receipt acceptance test previously used a blocking stdin
write and an unbounded process wait. A stuck receiver could hang the test suite.
The test now uses Tokio process I/O with one 30-second delivery deadline.
That deadline covers both input writes and process exit. On timeout or I/O
failure, cleanup kills and reaps the direct child with a separate three-second
deadline. Cleanup failure is explicit; it is not reported as successful delivery.
Kill-on-drop remains a fallback if the test unwinds.

Ordinary Linux regressions cover pipe backpressure, waiting for exit, successful
delivery, rejected delivery, and confirmed child reaping. The privileged test
still uses the real homelab Python receiver and checks Rust admission, delivered
revocation, stale bundle rejection, and checkpoint expiry.

This closes the deferred subprocess deadline gap from PR #33. It changes only
the test harness. It does not alter receipt authority, production receiver
timeouts, enrollment, transfer credentials, or deployment hooks. The receiver
does not intentionally spawn descendants; this helper does not claim process-tree
containment or recovery from an uninterruptible kernel operation.

The code uses existing Tokio 1.53.1 process and timeout APIs. No dependency or
tool version changes are required. See the
[Tokio process contract](https://docs.rs/tokio/1.53.1/tokio/process/struct.Command.html).

Validation uses the disposable Linux acceptance VM with synthetic receipts and
the reviewed producer fixture. A passing fixture is not live health provenance
or production U9 sign-off. Production activation still requires the worker and
authenticated source/transfer enrollment listed in the U9 staging status.
