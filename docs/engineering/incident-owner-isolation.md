# Incident and journal owner isolation

## Decision and evidence

The [SQLite contention diagnostic](../reviews/2026-09-30-sqlite-contention.md)
reproduced HTTP and timer stalls when the single-thread service executor waited
inside a synchronous journal write. The write still took about 450 ms off-thread,
but HTTP remained responsive.

Isolate the existing incident and journal owner on one dedicated OS thread with
its own current-thread Tokio runtime. Keep HTTP, the application heartbeat,
authenticated metrics, and incident-page reads on the original executor.

This is **not a database-only actor**. The owner includes dispatch, journal replay,
budget reservations, qualification, recovery, investigation, and notification
orchestration. These components share synchronous journal references. Keeping
them together preserves transaction order without introducing a partial async
storage facade or a second mutable journal snapshot.

## Ownership and admission

The owner constructs its journal and network dependencies on its own runtime.
Startup completes protected evidence import and stored-report loading before
signaling readiness. HTTP does not begin accepting application requests before
that signal. Startup errors and owner failure reject service startup.

The existing 64-entry intake queue remains the cross-thread admission boundary.
Its wait and acknowledgement deadlines are unchanged. The owner processes one
command at a time and sends the webhook acknowledgement only after existing
durable dispatch succeeds. A missing or cancelled response receiver does not
undo a submitted transaction or make it safe to repeat an external action.

The local operator service retains its existing four-entry command queue and
peer-credential checks. Its listener and tasks belong to the owner runtime.
No model-facing command, storage lock, or new execution authority is added.

## Shutdown and failure

Supervision requests shutdown through a watch channel. The owner checks that
request before accepting another incident command and after durable dispatch.
Pending asynchronous work can be cancelled; a synchronous SQLite call cannot.
A commit already running may finish and acknowledge before shutdown completes.
Queued commands that were not processed lose their acknowledgement channel
when the receiver drops; they do not receive fabricated success.

The supervisor waits up to ten seconds for owner completion. Completion follows
dropping the owner future and journal. A panic or missing completion is failure.
If the wait expires, `OwnerShutdownUnconfirmed` is returned. The owner may still
be running: restart the process through the service supervisor, not an owner in
the same process. There is no automatic in-process replacement or takeover.

Dropping the service future also requests shutdown but cannot await confirmation
from `Drop`. Abrupt process termination still relies on SQLite durability and
the existing durable reservation/recovery protocol, not this shutdown handshake.
This change does not add OS signal handling or prove graceful SIGTERM behavior.

Runtime shutdown waits 100 ms for auxiliary blocking jobs. A filesystem scan
already running can outlive that wait; it does not own the journal. No claim is
made that a timeout cancels filesystem I/O, remote work, or an uncertain validator
request. Existing operator recovery remains mandatory for uncertain outcomes.

## Tradeoffs and verification

This adds one owner thread and a separate Tokio runtime; their blocking pools
create threads on demand. It does not pin CPUs or create a runtime per incident.
SQLite contention can still delay owner-local provider timers, intake processing,
and operator replies. HTTP responsiveness does not mean an incident is progressing
or qualified. The process heartbeat now measures the HTTP executor, not owner
scheduling delay; receipt-journal timings retain their previous meaning.

The real owner-boundary test holds SQLite until HTTP and timer checks complete.
It verifies bounded queue backpressure, no premature commit acknowledgement,
an unconfirmed short shutdown deadline, rejection of queued work, and replay of
the committed event. A separate test checks panic and cancellation outcomes.

The actual service-binary test uses cleared environment variables, synthetic
credentials, fresh SQLite state, and loopback-only dependencies. It holds a
writer transaction, submits a webhook, confirms `/metrics` still answers while
the webhook waits, releases the writer, then checks HTTP 202 and durable replay.

Existing journal, budget, qualification, revocation, and manual-repair tests
remain regression gates. Production workload benchmarks, worker-local latency,
long-running filesystem failure, and graceful process-signal handling are separate
follow-ups. A future database-only actor should be considered only with evidence
that the remaining owner-local stalls matter.
