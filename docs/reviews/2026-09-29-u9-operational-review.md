# U9 operational integration review

The review covers the new receipt intake, checkpoint admission, incident dispatch,
and validator packaging on `feat/u9-operational-completion`, plus the companion
producer changes on `feat/u9-receipt-revocation` in bodhispace-homelab. Previously
reviewed rendering, worker isolation, and journal transactions were inspected
where the new integration depends on them; this is not a repeat of every earlier
review. The review and fixes were performed by the same coding agent, so normal
maintainer PR review remains required.

## 1. Must fix before continuing

Three code findings were fixed and verified:

- **P1: source capture could outlive checkpoint admission.** The dispatcher
  refreshed its checkpoint before calling a coordinator that captured Git again.
  A short-lived checkpoint could expire during that capture, yet the coordinator
  reserved work and invoked SSH. Admission now refreshes after capture, imports
  revocations, checks the candidate's qualification identity, and checks time
  again after reservation. The regression reproduced a consumed remote attempt
  under rejected admission; it now leaves no reservation. Final admission also
  passes the same checked time to the handoff transaction.
- **P1: revocation filenames overlapped completion identities.** A completed
  deployment named `revoked_deploy-1` occupied the revocation filename for
  `deploy-1`. Revoking the latter failed with `conflicting_revocation`. Revocation
  stems now contain a SHA-512 name digest and exceed the maximum deployment-ID
  length. The Linux regression preserves both completion receipts and publishes
  the third, distinct revocation event. This hash selects a filename; it does not
  authenticate evidence.
- **P2: transport validation ignored malformed receipt fields.** Canonical
  hashing discarded extra nested fields and accepted wrong JSON types. A receiver
  could publish files that Rust subsequently rejected, making the protected inbox
  unusable. Hashing now requires the Rust receipt structure, integer bounds,
  Boolean types, and pilot identity before publication. Tests reject extra policy
  and sample fields, malformed times, and foreign repository identity. A Linux
  receiver test confirms that a rejected bundle publishes no receipt or checkpoint.

No unresolved code finding from this review blocks the PRs. Before production
activation, the operator must enroll the worker and source/transfer identities,
connect trusted begin/complete/revoke hooks, and verify actual health provenance
and revocation delivery. These are release gates, not evidence supplied by the
isolated fixtures. [Dispatch operations](../../packaging/validator/DISPATCH.md)
defines the contract. U9 production acceptance remains incomplete.

## 2. Safe to defer

- Receipt retention and compaction: defer while bounded history fits 256 entries
  and the 1 MiB transfer limit. Overflow refuses delivery. The implementation must
  not delete revocations to restore availability. The operator owns escalation;
  design a reviewed compaction protocol before either limit is reached.
- Separate persistence workers for individual SQLite calls: receipt scans already
  run off the async runtime, and intake yields between commits. Journal ownership
  remains serial. Revisit this if measured commit latency delays incident or admin
  processing. Trusted local storage is still required.
- Automated GitHub publication: deferred by the accepted manual-publishing scope.
  Enabling it later requires the separate publishing gate and credential review.

## 3. Deliberate design decisions to document

- Root owns the producer and checkpoint publisher. Readable receipt bytes alone
  are not authority; the Linux reader checks protected paths and complete-set
  identity. Upstream Git and monitoring truth depend on the enrolled trusted caller.
- Freshness is bounded by the original upstream observation time. The destination
  cannot renew it. A change immediately after observation can remain unseen until
  transfer or expiry; choose lifetime and transfer cadence together.
- Delivery retains immutable receipts and publishes its checkpoint last. An
  interruption may cause refusal. Retry completes delivery, while an older bundle
  cannot hide a previously delivered revocation.
- Each incident gets one automatic reservation across runs and restart. Explicit
  recovery archives the uncertain attempt, but does not cause automatic retry.
- The application uses guarded dispatch. Public staged coordination helpers still
  require a trusted caller to establish source freshness and qualification. They
  are not exposed as model tools or unauthenticated endpoints.
- Artifact inspection reports historical data. Operator review, fresh source
  checks, repository CI, and human publishing remain required.

## Verification

Final local AI-SRE CI passed 231 tests, with two opt-in Tempo tests skipped;
formatting, Clippy, documentation, and cargo-deny passed. Homelab `make ci
TRUENAS_HOST=unused` passed. All 21 producer tests passed with
`--require-linux-root` in the isolated Fedora VM, with no skipped tests.
The revised guarded coordinator passed real loopback SSH, substantive offline
Podman validation, durable handoff, and restart duplicate suppression. The prior
full failure/restart/deadline/recovery acceptance remains recorded in
[U9 staging status](2026-09-08-u9-staging-status.md). These tests use synthetic
deployment evidence and disposable credentials; production services were not
changed. No crate, action, or validator tool version changed during this review.
