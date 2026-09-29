# U9 local enrollment preflight

## Scope

`ai-sre-validator --check-enrollment` checks local worker prerequisites before
an operator enables validation requests. The fixed SSH request protocol does
not expose this command.

The command checks protected configuration, worker-owned directories, the
pinned runtime, rootless isolation support, and the exact preloaded image.
It does not open the job store, reserve a request, or launch a validator.
Podman metadata commands can initialize local container storage.

## Verification on 2026-09-29

- Full local CI passed: formatting, Clippy, 235 tests, documentation, and
  dependency policy checks. Two opt-in Tempo tests were skipped.
- The real binary passed on the isolated Linux worker with rootless Podman.
  Hashes of both existing job and ownership databases remained unchanged.
- The real binary rejected root execution and rejected the local check flag
  when `SSH_ORIGINAL_COMMAND` identified a worker request.
- Regression tests reject unsafe directory permissions and symlink aliases.
  The image test requires the exact local digest and does not launch work.

## Remaining enrollment work

A successful check does not establish deployment health, source freshness,
the requested Git commit, pending-job state, or ledger capacity. Its JSON
output marks these unassessed properties explicitly.

Production still needs a selected worker, dedicated account, trusted host key,
pinned runtime and image, protected configuration, and authenticated receipt
transfer. A live handoff and revocation check must precede activation. No
production worker or deployment hook was enabled by this change.
