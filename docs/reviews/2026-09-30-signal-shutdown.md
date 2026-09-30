# Signal shutdown review

Scope: PR #36, the process signal handlers, application supervision, owner
completion, and durable intake tests. This is an implementation self-review,
not an independent approving review.

## Must fix before continuing

Resolved: worker failure paths sent a separate failure notification, then
returned `Ok(())`. Signal shutdown could observe that successful result before
the failure notification. Deployment refresh, operator service, dispatch,
reservation reconciliation, and report persistence failures now return errors
directly through owner completion. Intake exit uses the same checked shutdown
result. The redundant failure channel is removed.

A regression holds synchronous owner work, requests shutdown, releases the
operation, and verifies that its failure remains a failure. Another regression
cancels startup before readiness and verifies owned state release before success.
No remaining must-fix finding was identified in this review.

## Safe to defer

The binary signal tests exercise a started service, not every startup phase.
Startup cancellation is covered at the owner lifecycle boundary. A future
process test can inject a startup barrier and deliver a signal there.

The existing owner test verifies an unconfirmed short deadline. A separate
binary test of the full ten-second timeout can add coverage without changing
the shutdown policy. No production supervisor or container stop was exercised.

## Deliberate design decisions

Shutdown cancels asynchronous investigations; it does not drain the queue.
Synchronous SQLite calls can finish before the owner observes cancellation.
The listener closes before the supervisor waits for owner release. Existing
HTTP connections are not guaranteed to flush responses or finish all requests.
Durable retry deduplication remains necessary after uncertain acknowledgements.

Further signals do not force an early successful exit. SIGKILL bypasses the
handshake. The ten-second owner deadline is unchanged. Auxiliary filesystem
scans and remote operations are not covered by journal-release confirmation.
These limits are also recorded in the owner isolation engineering document.

## Verification

Full local `make ci` passed: 246 tests passed, three intentionally skipped.
Formatting, strict Clippy, doctests, rustdoc, and cargo-deny passed.
The real binary tests verify HTTP responsiveness under a SQLite writer lock,
commit-before-acknowledgement, SIGTERM followed by SIGINT during the blocked
commit, successful exit after release, and durable journal replay.
Hosted Linux CI must pass on the final PR commit before merge.
