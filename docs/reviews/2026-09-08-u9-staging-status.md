# U9 staging status

U9 is incomplete. This branch starts shadow-only preparation; it does not expose
a GitHub writer, accept production credentials, or authorize a deployment.

## Accepted scope change — manual publishing first

The operator accepted credential-free repair preparation with manual PR publishing.
No dedicated GitHub account, App, or Gate C signature is needed for this local
handoff. AI-SRE must not borrow the operator's personal `gh` credentials. The
canonical U9 plan records the revised acceptance boundary. U9 remains incomplete:
the current core has tested containment, substantive offline homelab validation,
and durable handoff preparation, but not operator-facing delivery or live worker
integration. The latest isolated image acceptance is recorded below.

## Implemented

- Corrected Gate C admission to require Gate A, independently of Gate B, matching
  the release plan's GitOps/runtime-mutation separation. Existing Gate C manifests
  naming Gate B will be rejected and must not be silently rewritten or re-signed.
- Added a deterministic pilot image renderer. It changes only the explicit
  `services.it-tools.image` scalar, preserves surrounding source bytes, and
  reparses the candidate against an expected one-field semantic change.
- Unsupported/ambiguous YAML and non-pilot or mutable references fail closed.
  Source size and candidate count bound parsing work. The renderer is not a
  deployment qualifier, repository validator, or authorization boundary.
- Tested indentation, scalar quoting, CRLF/LF, comments, duplicate keys, aliases
  affecting another service, missing pilot, unsupported flow layout, non-string
  images, and excessive candidate counts.
- Added pure paired-health assessment with configurable duration, maximum gap,
  and freshness. Reject empty, failed, unordered, duplicated, premature, future,
  stale, excessive, or overflowing evidence. A bounded final scrape may fall just
  after the window; checks cannot extrapolate success from before its end.
- Added exact-source preflight checks. Reject base movement, malformed SHAs,
  extra paths, and any bytes that differ from the deterministic renderer output.
  These are scope checks, not a claim that sandboxed validation ran.

- Added a Linux-only protected receipt reader. It rejects mutable/non-root
  paths, symlinks, hardlinks, non-regular files, oversized data, and invalid schema.
  It validates ownership up to the filesystem root. Root and the receipt producer
  are trusted; AI-SRE must run non-root. Receipts must use atomic replacement.
- Added atomic SQLite qualification and audit events. Replays are idempotent;
  contradictory receipts permanently invalidate an identity. Loading rechecks
  freshness and journal agreement. An injected audit-write failure rolls back
  the qualification row. A missing journal fact cannot be repaired by replaying
  the old receipt and pretending it was previously qualified.
- Added stored manual repair candidates bound to incident/run identities, source/base,
  evidence, and qualification digests. Candidates contain a one-line patch and
  a suggested PR title, relative incident-console link, journal-derived evidence
  citations, and controlled PR text, not the whole source file or raw health data. Image-line
  comments are rejected at artifact preparation to avoid copying arbitrary text.
  Candidate generation and its scoped journal fact commit together. The artifact
  explicitly states that sandbox/remote validation have not run and no PR exists.
  The patch uses zero context to avoid exporting unrelated source. A disposable
  Git fixture applies it with `git apply --unidiff-zero` and checks exact renderer
  output. This is not an instruction to apply it blindly: the future handoff must
  verify the recorded base and source digest first.
- Added bounded Git snapshot capture from the exact local HEAD. It reads tracked
  blobs without checkout hooks or filters, ignores local replacement refs, and
  excludes dirty files and Git metadata. It rejects symlinks, submodules, unsafe
  paths, excessive data, and a changed HEAD. Capture does not prove remote freshness.
- Added a fixed offline container plan with a digest-pinned validator image,
  read-only source, no network, no image pulling, dropped capabilities, and
  configurable resource ceilings. Preparing the plan requires the snapshot to
  match the candidate's base, source digest, and replacement image. The prepared
  value owns the temporary files and removes them when dropped. Preparation
  builds arguments only; runtime execution is a separate operation.
- Added a local Podman execution adapter. Admission requires a protected pinned
  Linux binary, rootless operation, cgroup v2 with CPU/memory/PID controllers,
  and seccomp support. The adapter rejects configured default host mounts,
  clears inherited process environment except required runtime locations, and
  disables image-declared volumes. Runtime helpers and host configuration remain
  deployment-trusted; binary hashing is not a complete runtime supply-chain proof.
- Validation checks bounded combined stdout/stderr, process success, and a clean
  container exit state, then requires forced removal before returning a receipt.
  Receipts bind candidate, snapshot, image, runtime, resource policy, output
  digests, output size, and completion time. They are not deserializable and do
  not grant publication authority or establish current deployment qualification.
- Cancellation retains the active job and snapshot in the runtime owner. Failed
  cleanup prevents that owner from admitting another job. The application must
  own one validator and call cleanup during shutdown. Dropping the owner or
  crashing is not a cleanup guarantee: the container deadline bounds a surviving
  workload, but stale-container recovery still needs application integration.
- Added validated handoff persistence. A successful sealed validator receipt must
  match a journaled candidate. Finalization rechecks base, scope, complete evidence
  identity, current qualification, and configurable validation age. Future results
  fail closed. Finalization also compares the exact validator image, runtime
  binary, and sandbox limits against independent deployment enrollment. A result
  cannot enroll itself. Handoff and its scoped audit fact commit together; an injected
  audit failure leaves no artifact. Replay, including after reopening SQLite,
  records no duplicate fact and still checks current inputs. The artifact requires
  operator review and records remote checks as not run. No PR is created.

The snapshot tests use real disposable Git repositories. A regression first
showed that local replacement refs could alter captured contents; disabling Git
replacement objects fixed it. Tests also cover dirty files, stale HEAD, committed
symlinks, mismatched repair images, exclusion of `.git`, and snapshot cleanup.
The runtime tests use a controlled local process, with a private test-only bypass
of Linux admission. They verify input binding, host/exit-state parsing, combined
output overflow, nonzero exit, silent-process deadlines, cancellation, and cleanup
failure. The cancellation test waits for the workload's start signal; an earlier
fixed-delay test was unreliable under local load. These tests do not prove runtime
isolation by themselves. The separate opt-in Linux acceptance tests below exercise
the real engine and filesystem boundary. A substantive pinned homelab validator
image and a real qualified deployment remain to be supplied.

The receipt reader proves local filesystem provenance, not upstream API truth.
The protected producer is not installed: it must independently join completion,
commit, actual image digest, predeclared policy, and paired observations. A model
cannot deserialize a protected receipt or a qualified deployment. Public health
and scope helpers alone still do not authenticate their inputs.

SQLite persistence tests use a private test-only protected-receipt fixture.
Non-Linux receipt imports fail closed. The Linux inbox test verifies ownership,
mode and link rejection against root-provisioned synthetic data; it does not
prove the producer's upstream observations or a deployed service configuration.
End-to-end protected-producer acceptance remains required before deployment.

The TDD loop first demonstrated `Gate C: InvalidPredecessor`, missing renderer
module, and acceptance of an excessive candidate count; implementation then
made those regressions pass. No claim is made that local renderer tests qualify
the full U9 workflow.

The next test-first slice demonstrated missing health/scope modules and rejected
bounded scrape jitter before implementation. Tests now cover both exact-boundary
and jittered coverage as well as stale-base and second-field failures.

The persistence slice first failed for the missing protected reader, qualification
methods, and manual-candidate API. The restore-divergence regression then failed
because a projection without its journal fact remained eligible; that defect was
fixed. Fault-injection tests verify transaction rollback with real SQLite, and a
real Git process verifies the generated patch in a disposable directory.

Latest validation: `NEXTEST_TEST_THREADS=2 make ci` passed formatting, strict
Clippy, 171 tests (two opt-in Tempo tests skipped), documentation, and cargo-deny.
Cargo-deny reported duplicate dependency warnings but all four checks passed.
During the earlier health/scope slice, a full-concurrency run passed 144 tests and failed two existing
one-second read-only process contracts under local load; both passed in the
reduced-concurrency run. Their deadlines and production code were not changed.
`git diff --check` passed.
Cargo added serde_yaml_ng and unsafe-libyaml; resolution also moved four existing
Windows dependency edges to the already-present windows-sys 0.61.2.

## Review resolutions — 2026-09-08

- Fixed missing-object snapshot reads that could invoke repository-configured
  host helpers. Capture now disables lazy fetching and supplies an empty Git
  protocol allowlist. A real Git fixture first reproduced an SSH helper running;
  it now rejects the missing blob without invoking the helper.
- Fixed source directory permissions under a restrictive service umask. Capture
  sets mounted source directories to 0755 and retains the outer directory at
  0700. A separate-process test first failed under umask 077 and now passes.
  This verifies filesystem permissions, not Linux container readability or isolation.
- Resolved replay policy: exact replays of a previously qualified receipt report
  current eligibility without writing another fact or invalidating the identity.
  Expiry and clock correction are not revocation. A failed first assessment or
  a contradictory receipt still permanently invalidates that deployment identity.
  Regression coverage checks expiry, a premature clock, and corrected-clock reuse.

- Fixed evidence ownership: candidate preparation checks the complete scoped
  evidence digest against durable journal facts inside the write transaction.
  Each new evidence fact records the canonical redacted record's content digest.
  Missing, legacy, duplicate, or foreign-scope facts fail closed. Reopen tests
  check stable identity; equal records in different runs have different bindings.
  This binds content identity; it does not add durable storage for raw payloads.
- Added candidate titles, relative incident links, and evidence citations from
  those same journal facts. Query text and tool payloads are not exported.
- Added fixed qualification outcome reasons and atomic retention of the initial
  and first contradictory receipts. Audit-write failure rolls back both receipt
  retention and qualification. Rejected receipts remain inspectable after inbox
  cleanup; an operator-facing audit reader remains to be wired.
- Fixed the scratch limit found during runtime review. The configured total now
  includes `/work`, 16 MiB for `/tmp`, and 16 MiB for shared memory. A 64 MiB
  budget gives `/work` 32 MiB, not an extra 64 MiB outside the configured budget.
- Bounded `mounts.conf` admission reads to 64 KiB and rejected nonregular paths.
  A regression first accepted oversized comments-only content and now rejects it.
- The real Linux fixture exposed read-only `/dev/shm` despite its allocated
  scratch budget. An explicit 16 MiB tmpfs fixed it without enabling Podman's
  extra default writable mounts. The original containment test then passed.
- Fixed a snapshot-directory collision found by repeated parallel handoff tests.
  A timestamp formatted in nanoseconds was not unique on the host. A fixed-clock
  regression reproduced `File exists`; adding a process-local atomic sequence
  fixed it while preserving exclusive creation and mode 0700. Ten subsequent
  four-test parallel handoff runs passed; the reviewer independently repeated
  the loop 20 times without failure. No existing directory is reused.
- Fixed validator enrollment at the handoff boundary after the follow-up review.
  A sealed success receipt alone was insufficient: a containment-only image could
  otherwise count as substantive validation. A regression first accepted an
  unenrolled image; finalization now rejects wrong images, runtimes, and otherwise
  valid but different limit sets. Qualification expiry/revocation still invalidates
  a candidate even when its validation receipt remains fresh.

The bounded follow-up review confirmed both the snapshot and enrollment fixes
with no new findings in that scope. This is not U9 release approval: the remaining
deployment and application integration below is still required.

## Linux acceptance evidence

An isolated local Fedora 44 OrbStack machine, `ai-sre-u9-validation`, has no
host-file sharing or SSH-agent forwarding. No homelab service was changed.
Tests ran as non-root user `validator`, not root. They used Podman 6.1.1 built
from its upstream tag, netavark 2.1.0, and crun 1.28. Fedora's old storage defaults
were replaced with Podman 6's shipped defaults; its default RHEL-secret mount
was preserved under a disabled filename in this disposable machine only.

- All 13 ordinary U9 integration tests passed on Linux.
- A later Linux GitOps-filtered unit run passed 18 tests, including durable
  handoff, rollback, and reopen checks. Its opt-in containment test was skipped
  by the ordinary runner and passed separately as recorded below.
- The explicit protected-inbox test accepted a root-owned regular receipt and
  rejected writable files, user-owned files, symlinks, hardlinks, and writable
  parent directories. Every receipt was synthetic.
- The explicit containment test used the production Rust admission, launch,
  inspection, and cleanup path with a digest-pinned local fixture. It verified
  UID 65532, zero effective capabilities, no-new-privileges, seccomp, loopback-only
  interfaces, denied source/root writes, and writable bounded scratch. Kernel
  cgroups reported 512 MiB memory, zero swap, 128 PIDs, and one CPU quota.
- The receipt was returned only after container removal and snapshot cleanup.
  Temporary diagnostic logging was removed, then the test passed again.
- After the enrollment fix, the real containment test also confirmed that its
  successful receipt cannot finalize when another validator image is enrolled.

No containers remained after acceptance. The disposable machine was stopped to
release resources; its disk and installed test tools remain available for reuse.

The fixture did **not** run homelab `make ci`; it cannot qualify a real repair.
See [fixture instructions](../../tests/fixtures/u9-sandbox/README.md). Linux builds
used Fedora's Rust 1.98.0 package; the project toolchain remains Rust 1.95.0 and
the full local CI result uses that pin. No claim is made of full Linux CI.

Recorded local runtime digest:
`sha256:ec67365f44d1abae5f9ed45b5709b520dc686d046826a7bd2a859b66ece62371`.
Recorded containment-only image digest:
`sha256:458ec8b04ca346acea965c8fae703a219b0888b59656857746ad059957be664e`.
These are test identities, not production trust configuration.

Qualification facts remain global; candidate facts carry incident/run scope.
Candidate preparation still grants no validation, publication, or deployment
authority. The trusted producer, not the receipt reader, establishes upstream truth.

## Gaps for future privileged operation

- `VerifiedPromotionManifest` currently has only a test constructor path; there
  is no production Cosign invocation, packaged verifier, or image-pinned trust
  root. This remains a deployment/promotion gap, but does not block credential-free
  local preparation and testing.
- Gate C must bind fork identity, machine-user role, credential scopes, workflows,
  pilot path, base policy/SHA, renderer, and qualification policy. The existing
  generic promotion manifest does not explicitly represent those bindings.
- Automated-publishing acceptance requires a disposable non-deploying repository, bot-owned
  fork, separate appropriately scoped identities, revoked canary credentials,
  and an operator-signed manifest. Existing developer credentials are not a
  substitute and must not be used to test denied writes against production.

## Remaining implementation

Read-only inspection of the current homelab deployment workflow found a latest-
Gatus-result health gate and a successful-commit cursor. These are not the strict
post-completion paired observation window or deployed-image receipt required by
U9. Do not promote them to a qualified rollback record. A protected ingestion
path must join completion, actual image digest, commit, policy, and health samples.

Protected-producer integration and deployment inbox acceptance; application wiring and
remote freshness checks for repository capture; production validator image enrollment,
single-owner wiring, shutdown cleanup and stale-job recovery; operator
artifact delivery and candidate recovery UI; detailed efficiency projection;
end-to-end manual handoff tests. Keep publication separate from artifact readiness.

The next deployment boundary needs a separate homelab change: a protected receipt
producer and a credential-free Linux validation worker, with independent runtime,
substantive image, and policy enrollment. Do not make the AI-SRE container
privileged to launch nested containers. The operator approved preparation in a
separate homelab worktree. Draft
[homelab PR #487](https://github.com/bodhispace-xyz/bodhispace-homelab/pull/487)
stages a dormant protected producer and records the remaining activation gates.
No deployment, merge, or privilege change was performed or authorized.

The producer freezes intent and policy, checks fixed Docker identity and image
digest, assesses bounded Gatus/Prometheus history, and publishes protected local
receipts. Failures and interrupted collections cannot silently restart a window.
All 14 synthetic producer tests passed in the isolated Linux VM, including four
root-filesystem/state tests. Full homelab local CI and the PR's GitOps Plan &
Validate check passed; Apply & Deploy was skipped. The VM is stopped.

This is a staging foundation, not live producer acceptance. Prometheus currently
scrapes Gatus, so these sources are correlated rather than independent probes.
Trusted commit-label hooks, live source enrollment, inbox delivery/revocation,
and end-to-end acceptance remain. The remote worker is not provisioned: current
Rust validation runs locally and needs an authenticated transport and lifecycle
integration before a separate worker can be activated. No shell/Python duplicate
of the Rust validator was introduced. Review the producer's privileged boundary
at Sol/high before activation; keep U9 marked incomplete.

September 9 follow-up: Sol/high found a completion-second health failure bypass
and fail-closed publication/CI gaps. Homelab commit `1734ead` fixes these in the
same PR: ambiguous boundary-second failures reject qualification; Linux atomic
no-replacement rename replaces hard-link publication; a separate credential-free
hosted Linux-root job rejects skipped coverage; and the plan check explicitly
fails on unsuccessful dependency results. Hosted Ansible syntax checks now use
the shared local recipe. Recovery documentation states the remaining power-loss
durability limits. Both defect regressions first failed and now pass. All 16
producer tests passed in the isolated Linux VM with the strict no-skip gate,
the workflow failure-propagation regression passed, and full homelab local CI
passed. Sol/high's focused re-review found no new regressions. The local VM is
stopped. Hosted `AI-SRE Linux Receipt Contracts` and `GitOps Plan & Validate`
both passed for `1734ead`; `GitOps Apply & Deploy` was skipped. No production
deployment or merge was performed.

Read-only inspection of the current homelab `main` Makefile confirms substantive
inventory, OpenTofu, Ansible, Compose, and script checks. The validator image must
preload their dependencies, including Terraform-provider packages, so `make ci`
can run without network access. The local homelab checkout is older than this
Makefile and must not be used as evidence that CI recipes are missing.

The candidate schema deliberately has no publish-ready state. Do not add a
caller-supplied `validation_passed` boolean to bypass the future validator.

The GitHub write adapter, automatic reconciliation/check tracking/branch cleanup,
privileged workflow extension, and Gate C verifier/canary are deferred.

## Homelab merge follow-up

Merge follow-up: the operator merged homelab PR #487 on September 9, 2026,
as `fafa318148e5046ef175072b4610ca125203d20b`. Homelab `origin/main` was fetched
without changing the existing worktree or the uncommitted AI-SRE U9 work.
The resulting [GitOps run](https://github.com/bodhispace-xyz/bodhispace-homelab/actions/runs/34402659618)
completed successfully, including OpenTofu apply/state unlock, Ansible deployment,
the post-deployment health gate, and the successful-deployment record. The two
PR-only validation jobs were skipped as expected on the main-branch push; their
pre-merge results are recorded above. A separate direct container check could not
run because the local SSH hostname `utility` did not resolve, so this verification
relies on the deployment workflow rather than a direct host inspection.
The producer remains dormant because the normal deployment
playbook does not import its staging tasks. The next U9 implementation work is
Rust worker/lifecycle integration after the offline image acceptance below;
live producer hooks and receipt delivery still require their acceptance checks.

## Offline validator acceptance — 2026-09-09

Added [validator packaging](../../packaging/validator/README.md) in AI-SRE.
No new homelab change or PR was needed. The build uses a clean archive of homelab
`fafa318148e5046ef175072b4610ca125203d20b`, copies only locked provider inputs
from that archive, and preloads the tools, provider mirror, and Ansible collections.
The image runs full homelab `make ci` without network access or credentials.
The entrypoint clears inherited Make options and uses a new scratch tree; it
does not duplicate the Rust runtime's admission, cleanup, or receipt logic.

Three ordinary entrypoint tests cover source preservation, failed checks, and
refusal to reuse scratch. They also inject a Make dry-run flag to check that it
cannot suppress validation. The first test failed before implementation.
Real offline execution then exposed non-executable scratch: repository scripts
failed with permission denied. A Rust plan regression first failed, then passed
after explicitly setting `exec` on `/work`. `/tmp` and `/dev/shm` stay `noexec`.
The original real-Linux containment assertions passed again after this change.

The final image, with online-verified Docker CLI 29.8.0 and Compose 5.5.1, passed
the production Rust `RootlessValidator` path in the isolated Fedora VM. It used
default resource limits and a synthetic qualification with the deterministic
pilot-image replacement. The full repository passed in 6.85 seconds. A separate
committed repository containing invalid OpenTofu syntax was rejected in 1.12
seconds. Both tests checked container and snapshot cleanup. These are single
test observations, not benchmark results or proof of live deployment health.
A separate network-disabled container check hid the provider mirror and failed
at provider discovery, with no direct-download fallback.
No acceptance containers remained afterward. The isolated VM was stopped;
its test files and images remain available for reuse.

Final local Linux/ARM64 image identity:
`sha256:f332f228a5e5b5399267f474590c20be89791d53903d11a7ae0f119f668bc9cd`.
Runtime identity remains the isolated Podman digest recorded above. Neither is
production enrollment. The image has not been pushed to GHCR. Only Linux/ARM64
image acceptance was exercised; an AMD64 worker needs its own image acceptance.

Final local `NEXTEST_TEST_THREADS=2 make ci` passed 174 tests, formatting, strict
Clippy, doc tests, documentation, and cargo-deny. Two opt-in Tempo tests skipped.
The initial restricted run could not bind sockets in five existing tests; the
rerun with local socket access passed. No socket-test behavior was weakened.

Review before production enrollment: OpenTofu reports an expired signing key
for the homelab's locked Proxmox provider while still authenticating the package.
No verification bypass or provider upgrade was introduced. OS and transitive
tool dependencies resolve at build time and are inventoried in the final image;
the build is not byte-reproducible. Every rebuilt image requires review and fresh
digest enrollment. A focused review should cover these supply-chain limits,
the executable scratch policy, and the distinction between local acceptance
and production activation before worker deployment.

U9 remains incomplete: authenticated remote worker transport, single-owner
lifecycle and crash recovery, live producer hooks/inbox delivery, repository
freshness, operator delivery/recovery, efficiency projection, and end-to-end
manual handoff acceptance remain. Existing uncommitted U9 work is preserved.

## Review follow-up — negative acceptance and shutdown

Strengthened negative acceptance to require a bounded diagnostic naming
`invalid-fixture.tf` and its expected OpenTofu `Invalid block definition` error.
An unrelated failure cannot satisfy that assertion. The diagnostic capture exists
only in test builds; production still exports no raw workload output. The new
regression first failed before implementation. Real Linux acceptance then exposed
an incorrect expected error string, which was corrected against the actual parser
diagnostic. The invalid repository passed the strengthened assertion; deliberately
selecting a missing image failed that assertion, as required.

Added `RootlessValidator::validate_until_shutdown` as a lifecycle integration
primitive. An already-ready shutdown prevents launch. Shutdown during validation
cancels the workload and awaits cleanup before returning an error, never a success
receipt. Failed cleanup retains the active job and snapshot for retry. The caller
must await the method and retain the owner on failure; aborting its task is still
not a shutdown guarantee. Process tests cover pre-launch shutdown, active shutdown,
cleanup failure/retry, and normal completion. The shutdown regression first failed
before implementation. This does not yet wire the application, implement the
remote transport, or recover containers after a process crash.

Final full local CI passed 177 tests, formatting, strict Clippy, documentation,
doc tests, and cargo-deny; two opt-in Tempo tests skipped. The final real Linux
offline success and expected-rejection tests also passed through the new method
with a pending shutdown signal. They took 7.07 and 1.14 seconds respectively,
not benchmark measurements. No image rebuild, publication, production change,
or new PR was needed. Application wiring must use cleanup-and-join supervision,
not the existing incident worker's task-abort path.

## Durable worker recovery checkpoint (2026-09-10)

Added opt-in persistent ownership through `RootlessValidator::connect_with_state`.
A separate SQLite exclusive lock admits one owner per private state directory.
The job journal commits launch intent before spawning Podman and preserves a
monotonic name sequence across restarts. No new dependency was needed.

Recovery checks the recorded name and random ownership label. It commits the
immutable container ID before removal, so a crash between removal and journal
cleanup can be reconciled. Snapshot cleanup checks the recorded path, owner,
device, and inode and syncs the parent before clearing the record. Snapshot Drop
also checks device/inode to avoid deleting a replacement at the same path.

An absent container with no previously observed ID is ambiguous: an interrupted
launcher may still create it. Recovery retains that record and blocks admission.
This also means an early launch failure can require operator reconciliation.
Do not describe this as complete automatic crash recovery. Supervisor-assisted
resolution of launch uncertainty, authenticated remote transport, and application
cleanup-and-join wiring remain unfinished. The lock protects one state directory;
deployment must not give competing workers different state directories for the
same runtime. State migration and recovery tooling need review before deployment.

Verification:

- Process tests cover abrupt exit without destructors, lock release, durable
  intent, conflicting ownership, immutable-ID removal, unknown launch absence,
  known-ID absence, and refusal to remove a replacement snapshot.
- Real rootless Podman restart recovery passed in the isolated ARM64 VM. The
  recorded container was removed while an unrelated container remained running.
  The test then removed its unrelated fixture container.
- Final real offline homelab acceptance passed for both the valid source (7.32 s)
  and the specific invalid OpenTofu fixture (1.57 s). These are test durations,
  not benchmarks. Both used persistent ownership and verified cleanup.
- Full local CI passed with `NEXTEST_TEST_THREADS=2`: 180 tests, formatting,
  strict Clippy, documentation, doc tests, and dependency checks. Two opt-in Tempo
  tests skipped. The initial default-concurrency run had seven failures and one
  leaky-test warning; all passed at reduced concurrency. Timing sensitivity under
  load remains a review item, not a claim of an unconditional default-CI pass.

This is a worker-lifecycle review checkpoint, not U9 completion or production
readiness. No PR, image publication, credentials, or production deployment changed.

## Worker recovery review fixes (2026-09-10)

Both must-fix findings from the worker-lifecycle review are addressed:

- **Runtime/storage binding:** recovery now checks a persisted identity before
  trusting container absence or removing anything. It includes Podman's resolved
  graphroot/runroot, driver, canonical home/runtime paths, and graphroot device,
  inode, and owner. Transient storage is rejected. Runroot inode changes on reboot
  are allowed. A pending legacy journal without a binding fails closed; enrollment
  cannot silently attach it to the current runtime. Configuration must remain
  stable while the worker runs; this is not protection against a hostile host.
- **Confirmed spawn failure:** an OS spawn error now removes the recorded snapshot
  and clears intent synchronously. No process started, so the worker can accept a
  subsequent job. A Podman process that starts and exits unsuccessfully is still
  an uncertain launch; the conservative recovery rule remains unchanged.

Three regression tests cover persistent binding rejection after reopen, changed
storage with a known-but-apparently-absent container, and an actual permission-based
spawn failure followed by another launch. The spawn regression failed on the
uncleared intent before the fix. Full local CI passed with
`NEXTEST_TEST_THREADS=2`: 183 tests, formatting, strict Clippy, documentation,
doc tests, and dependency checks; two opt-in Tempo tests skipped. Real rootless
restart recovery also passed with the storage binding enabled. Final real offline
acceptance passed for valid source (8.55 s) and expected invalid-fixture rejection
(2.79 s). No test containers remained afterward. These durations are not benchmarks.

Deferred items remain: default-concurrency timing sensitivity, explicit journal
schema versioning and operator recovery tooling, and crash injection at more
launch/removal boundaries. These fixes do not implement remote transport,
application wiring, or complete U9. No new PR or production changes were made.

## Authenticated worker integration (2026-09-10)

Added the `SshValidator` client, bounded versioned request/response protocol,
fixed `ai-sre-validator` SSH entry point, and
`application::manual_repair::validate_manual_repair` coordinator. The client uses
a dedicated key, explicit known-hosts file, strict host checking, no user SSH
configuration or agent, and no forwarding. Deployment must enforce the fixed
command on the server; building this code does not provision that authority.

The worker independently enforces its root-owned configuration, captures the
requested HEAD from a deployment-owned mirror, checks the snapshot digest, and
uses the existing durable rootless runner. Request IDs are consumed before work
and survive reconnects. Replay is rejected, not rerun. Retention is bounded at
4096 records and fails closed at capacity; reviewed pruning/recovery remains open.
The worker imports no responder journal, OAuth data, GitHub credentials, or keys
into validation containers.

Only an authenticated successful SSH exchange with matching request, candidate,
snapshot, image, runtime, policy, bounded output metadata, and time can construct
an imported receipt. The application coordinator recaptures local HEAD and calls
the existing transactional qualification/evidence-aware handoff finalizer.
The artifact remains `operator_review_required`; repository remote checks remain
`not_run`, and no publication or deployment authority is granted.

Full local CI passed with two test threads: 187 tests, formatting, strict Clippy,
documentation, doc tests, and dependency checks; two opt-in Tempo tests skipped.
The isolated Linux acceptance exercised a real localhost SSH forced command,
substantive offline validation, and journaled application handoff. Wrong host-key
enrollment and replayed requests were rejected. An initial setup error copied the
worker configuration with UID 501; the worker correctly rejected it until its
ownership was corrected to root.

The test used disposable client/host keys, no SSH agent, and a listener bound only
to 127.0.0.1:22222. Fedora's latest offered server was 10.2p1-14.fc44; upstream was
10.5 when checked. This is isolated test evidence, not production version or
debug-binary enrollment. No new Rust dependencies were added. `default-run`
preserves existing `cargo run` behavior after adding the second binary.
The final real-channel acceptance took 18.53 seconds, not a benchmark. No containers
remained. The test listener was stopped, both disposable private keys and their
authorization file were deleted, and the VM was stopped. The package-created
default sshd boot enablement was undone; no SSH service was left enabled by this test.

Review boundary: the callable application coordinator is tested but not enabled
in the incident loop or exposed through an operator endpoint. Production SSH
provisioning, protected producer ingestion, remote Git freshness, operator artifact
delivery, process-signal supervision, and uncertain-result recovery remain open.
A broken connection is not proof of remote cleanup; work remains deadline-bounded
and durable recovery applies. No automatic retry is performed. U9 is incomplete.
See [worker deployment contract](../../packaging/validator/WORKER.md).

## Transport review fixes (2026-09-10)

- **SSH trust paths:** the client now checks the entire credential/known-hosts
  path chain before spawning SSH. Files and parents must belong to root or the
  effective service UID. Symlinks, untrusted owners, writable parents, and hard-linked
  files are rejected. Root-owned sticky ancestors remain allowed. `rustix` 1.1.4
  supplies the effective UID through a safe API; its latest version was checked
  online before adding the dependency. Root and same-UID processes remain trusted.
- **Application retries:** one atomic, durable attempt reservation per candidate
  stores the exact request before dispatch. A restarted or competing caller cannot
  replace it with a fresh nonce. Transport failure or cancellation retains the
  uncertain attempt. A received sealed receipt is recorded as validated before
  source recapture and final handoff checks. Both uncertain and validated attempts
  reject implicit redispatch with `AttemptExists`. Audit JSON cannot mint a sealed
  receipt or skip freshness checks.

Regression coverage checks symlinked/writable credential parents, persistence of
the original nonce across reopen, and application rejection of both uncertain and
validated attempts before repository/SSH access. No reset endpoint was added:
authenticated operator recovery/revalidation remains a production prerequisite.
Other deferred items remain unchanged, including non-regular worker configuration
rejection, replay-ledger retention, expanded disconnect/signal tests, and activation.
Verification: full local CI passed with `NEXTEST_TEST_THREADS=2` (190 tests,
formatting, strict Clippy, documentation, doc tests, and cargo-deny; two opt-in
Tempo tests skipped). Linux passed 26 focused sandbox tests plus the durable
attempt-reopen test; three opt-in live container tests were ignored. The live SSH
fixture was not rerun for these fixes, and no credentials or listener were created.

## Local operator recovery and artifact access (2026-09-10)

Added the opt-in Unix admin listener and `ai-sre-admin inspect/recover` client on
the same U9 branch. The application-owned journal worker handles requests serially.
No HTTP admin route or model tool was added. The listener is disabled by default.
Deployment must enroll a protected socket directory and, optionally, the numeric
`ai-sre-operators` group ID. Authorization uses kernel peer UID/effective GID;
supplementary-group membership alone does not grant access.

Inspection returns original candidate data, including patch and proposed PR text,
active attempt data, and the latest 100 recovery records. It explicitly does not
assess current readiness or restore a trusted receipt from stored JSON.

Recovery requires an expired request, its exact digest, and a bounded reason. One
transaction archives the old attempt and operator identity before clearing the
active projection. Replayed recovery requests cannot clear newer attempts, and
archived requests cannot be reserved again. Completion now compares the exact
reserved request, preventing late responses from completing a different attempt.
Recovery never launches work or proves remote cleanup. The existing worker and
handoff checks still apply on a subsequent explicit validation call.

Recovery history is an append-only application API in the same SQLite journal,
not an incident-event projection. Metrics and incident-page projection remain
open. Current candidate readiness, candidate listing, worker-side uncertain-job
reconciliation, replay retention, and incident-to-validation activation remain
open. See [the operator runbook](../../packaging/validator/OPERATOR.md).

Verification: full local CI passed with `NEXTEST_TEST_THREADS=2`: 197 tests passed,
two opt-in Tempo tests skipped; formatting, Clippy, docs, doctests, and cargo-deny
passed. Socket tests required execution outside the filesystem sandbox. An initial
disconnect fixture failed because it closed the peer before authentication; the
corrected test authenticates first, loses the reply, and verifies durable history
after reopen. Linux library tests as the unprivileged validator user passed:
75 passed, three opt-in live-container tests ignored. No live SSH/container
acceptance or production deployment was performed in this slice.

This is a deep-review boundary before activation, not completion of U9. Review
the admin identity/path boundary, transaction and late-response races, bounded
request handling, and the distinction between recovery eligibility and readiness.

## Operator review fixes (2026-09-10)

Resolved the two operational findings and the cross-user acceptance gap from the
focused operator review:

- Socket I/O now runs in a bounded background service: four client tasks and a
  four-command queue. The incident worker executes only decoded commands and
  never waits for client reads or writes. Journal mutations remain serialized.
  Queued commands carry a monotonic five-second deadline checked before execution.
- `ai-sre-admin recover` now exits unsuccessfully when recovery is declined or
  success is not confirmed. The received JSON remains available on stdout.
- Added Linux acceptance using the actual CLI under existing validator/nobody
  identities. Root and the effective-primary-group operator succeed. Non-operators
  fail, including a supplementary-group-only user who can access the socket file.
  The accepted recovery records the operator's kernel UID/GID; replay fails.

The CLI exit regression was observed failing before the fix. The slow-client
test proves a complete request finishes before an incomplete client's read
deadline. A separate regression prevents expired queued recovery even if its
reply timeout has not yet been polled. The Linux cross-user acceptance passed
explicitly as root in the disposable VM; it was not counted as an ordinary skipped
test. No accounts, production services, or credentials were changed.

Production activation remains disabled by default. Existing design boundaries
and outstanding U9 work remain unchanged. The operator runbook now describes the
queue, exit status, timeout outcomes, and the privileged acceptance procedure.

Final verification: local `NEXTEST_TEST_THREADS=2 make ci` passed 200 tests, with
two opt-in Tempo tests skipped; formatting, Clippy, documentation, doctests, and
cargo-deny passed. Linux library tests passed 77 tests; three live-container tests
and the separately executed cross-user test were ignored in that ordinary run.
The VM lacks the Cargo Clippy component, so Linux-specific Clippy was not run;
the Linux-only test code was compiled and its privileged acceptance executed.
No dependencies were added. The disposable test fixtures were removed and the
VM was stopped after acceptance.

## Incident-to-handoff integration acceptance and review (2026-09-11)

The explicit `run_incident_repair` entry point now derives a candidate from current
durable deployment qualification, incident/run evidence, and immutable local Git
source, then calls the existing remote validator and transactional handoff path.
Preparation reconstructs the same candidate after journal reopen; it does not
deserialize stored JSON into authority. Operator inspection returns the most
recently stored handoff as historical data, with `readiness: not_assessed`.

The live Linux test
`incident_failure_restart_recovery_and_handoff_over_real_transports` passed on
2026-09-11: one passed, zero failed, zero ignored, in 192.21 seconds. The earlier
run's terminal output was unavailable, so this is a fresh observed result, not an
inferred success. Source hashes for the coordinator, admin adapter, and flow test
matched between the workspace and the disposable VM.

The test used the real localhost SSH forced-command worker, pinned rootless
Podman/offline image, and actual operator CLI. It covered:

- Failed host authentication leaves a durable uncertain attempt.
- Journal reopen does not allow implicit redispatch; early recovery fails.
- Recovery waits for the real request lifetime, preserves history and kernel UID,
  and rejects a replayed recovery request.
- Explicit revalidation uses a fresh nonce and passes the substantive offline
  validator before journaling the handoff.
- Operator delivery after reopen returns that exact handoff; remote repository
  checks remain `not_run`, and inspection does not claim current readiness.
- Later qualification revocation blocks the complete entry point without erasing
  historical handoff data.

Qualification and incident evidence were synthetic. Restart coverage here means
reopening SQLite, not killing a running responder. This is not live Alertmanager
intake, production receipt ingestion, upstream Git freshness, or production
activation acceptance. These distinctions are explicit in the operator runbook.

Full local `NEXTEST_TEST_THREADS=2 make ci` passed again: 201 tests passed and two
opt-in Tempo tests skipped; formatting, Clippy, docs, doctests, and cargo-deny
passed. No dependencies changed. After the live test, Podman reported no remaining
containers. The dedicated listener on port 22222 was stopped; its host private
key, client private key, and authorized-key file were deleted. Public fixture
files and worker replay state remain. The disposable VM was stopped.

### Prioritized review outcome

1. **Must fix before continuing:** no new blocking defect identified in this
   integration slice's qualification, attempt, recovery, or handoff boundaries.
   This is not full U9 or production sign-off. Previously documented activation
   prerequisites remain open.
2. **Safe to defer:** preparation and validation capture the repository separately;
   reducing this duplicate work is an optimization, not grounds to drop the final
   source recheck. Full responder process-kill/restart acceptance, current-readiness
   presentation, candidate listing, and efficiency-event projection remain future
   work. Existing worker recovery/provisioning gaps are not closed by this test.
3. **Deliberate design decisions:** the trusted caller chooses incident/run,
   deployment ID, repository, and observed base; the application derives source,
   replacement image, and evidence binding. Recovery reopens eligibility rather
   than dispatching work. Stored handoffs remain historical after recovery or
   revocation. Automatic alert-loop activation and GitHub writes remain disabled.

Review examined the new preparation/delivery code together with the existing
qualification, attempt CAS, admin queue, receipt import, and finalization paths.
It did not repeat a repository-wide audit of already reviewed milestones. No PR,
commit, push, or production deployment was performed in this verification slice.

## Repair progress metrics (2026-09-11)

The efficiency projection now counts durable preparation and validated-handoff
facts per incident/run. SQLite replay rebuilds the same counts after restart.
The existing metrics endpoint exposes both as fixed aggregate counters, without
incident IDs or artifact digests as labels. Repeated finalization and retrieval
do not add handoff facts or increase the count.

These counters describe history, not current readiness. A validated handoff does
not prove delivery, publication, merge, deployment, or incident resolution.
Expiry and qualification revocation do not erase earlier handoff facts. No time
is inferred from preparation and handoff wall-clock timestamps: that gap can
include downtime and operator delay, not just machine work.

This is partial efficiency coverage. Validation-attempt, recovery, cleanup, and
human-wait facts remain to be added before full U9 timing can be reconstructed.
No production activation, dependency change, or GitHub write is part of this work.

Validation: the replay/metrics regression test passed after failing for the
missing projection fields and then the missing exported counters. Full local
`make ci` passed with socket access: 202 tests passed and two opt-in Tempo tests
were skipped. Formatting, Clippy, documentation checks, and cargo-deny passed.
The earlier sandboxed run could not complete socket-dependent tests. Linux
root-only and live SSH acceptance were not rerun for this projection-only change.

## Validation lifecycle journal follow-up (2026-09-11)

Reservation, receipt storage, and accepted recovery now each commit a
`ManualValidation` fact in the same SQLite transaction as their state change.
The fact contains artifact and request digests, a typed stage, and the source
Unix timestamp when available. Raw requests, operator IDs, and recovery reasons
stay out of this event; the existing protected recovery audit retains its detail.

The journal rebuilds three counters after restart: reservations, stored receipts,
and accepted recoveries. The metrics endpoint exposes fixed aggregate names with
no incident, artifact, request, or operator labels. Duplicate reservations,
replayed recovery requests, and repeated receipt completion do not add facts.

Scope comes from the candidate's durable preparation fact, not a caller-supplied
incident ID. Contradictory scopes fail closed. Legacy attempts without a provable
scope remain unscoped. Existing attempt and recovery rows are not backfilled, so
these counters cover recorded transitions, not all work before this change.
Older binaries that do not recognize the new event cannot replay a journal after
it receives these events; downgrade requires a compatible reader.

A reservation does not prove SSH dispatch. A receipt does not prove current
handoff eligibility. Recovery permits another reservation but proves neither
remote cleanup nor a new dispatch. These counters cannot be subtracted to infer
the number of uncertain jobs: recovery may archive either an uncertain or a
validated attempt, and older reservations may lack journal facts. Unix timestamps
remain audit times, not machine or human-wait durations.

Tests cover SQLite event-write failure during all three transitions, duplicate
and rejected requests, contradictory scopes, stale journal owners, and scoped
replay after reopening. Remaining work includes measured validation durations,
worker cleanup facts, human-wait boundaries, and full responder process-kill
acceptance. Production activation and GitHub publishing remain disabled.

Validation: full local `make ci` passed with socket access: 204 tests passed and
two opt-in Tempo tests were skipped. Formatting, Clippy, documentation checks,
and cargo-deny passed. The additional stale-owner assertion passed in a focused
rerun after the full suite built. No dependencies changed. Linux root-only and
live SSH acceptance were not rerun for this change.

## Lifecycle review and successful-response timing (2026-09-11)

The focused review checked transaction ordering, rollback, cached journal
sequences, scope derivation, duplicate handling, and legacy replay. It found no
new must-fix defect in those paths. This is not a full U9 or production review.

The responder now measures its live wait around `worker.validate` with a
monotonic clock. On success, it commits `response_elapsed_ms` with the receipt
fact. This includes the SSH/worker round trip, not just validator execution. It
excludes local snapshot preparation, journal I/O, post-validation checks, and
operator wait. Failed or cancelled waits do not produce timing samples yet.

Replay retains both the sum and sample count. Older facts without the optional
field remain readable and do not count as measured zero. A measured duration
below one millisecond does count as a sample with zero milliseconds. Fixed
aggregate metrics expose the sum and timed-receipt count without new labels.
Use the timed-receipt count, not all reservations or receipts, as the denominator
for a successful-response average. This average cannot describe failed attempts
or total incident-resolution time.

Prioritized review outcome:

1. Must fix before continuing: no new actionable defect found in this scope.
2. Safe to defer within dormant staging: failed/cancelled wait measurements,
   worker cleanup timing, human-wait boundaries, and complete process-kill
   acceptance. These remain gaps before full U9 efficiency acceptance.
3. Deliberate decisions: no invented legacy history or durations; recovery is
   permission to retry, not cleanup proof; response timing is separate from
   CPU time and total repair time. Production activation remains disabled.

Tests cover legacy decoding, measured zero, wall-clock changes that do not alter
measured duration, atomic timing rollback, duplicate receipt rejection, and
scoped SQLite replay. No dependencies or worker wire protocol changed.

Full local `make ci` passed with socket access: 205 tests passed and two opt-in
Tempo tests were skipped. Formatting, Clippy, documentation checks, and
cargo-deny passed. Live SSH and Linux root-only acceptance were not rerun.

## Combined efficiency and recovery batch (2026-09-11)

This batch adds measured failed-response facts without clearing uncertain
reservations. Duplicate error callbacks count once. A failed fact insert adds no
metric, and a new nonce still requires explicit recovery. Successful and failed
responses have separate duration sums and measured-sample counts. Cancellation
or abrupt process loss leaves the original reservation intact; no error,
duration, or cleanup outcome is invented after a missing response.

Operator inspection now reports cleanup as confirmed by the stored authenticated
success receipt, or unknown. This is historical evidence for the active attempt.
The worker already withholds success receipts until cleanup succeeds. The
responder cannot distinguish a worker cleanup failure from a lost response, so
no negative cleanup claim is inferred from SSH errors or operator recovery.

The authenticated local CLI gains `acknowledge SOCKET HANDOFF_DIGEST`. This closes
the handoff-creation-to-pickup wait exactly once and records kernel UID/GID. It
does not approve a repair, publish an artifact, or dispatch validation. Inspection
returns the exact digest and any acknowledgement without acknowledging it.
Pickup wait uses server wall timestamps across restarts, not monotonic execution
time. Clock regression gives an unknown duration; forward clock changes can skew
this wall measure. It is kept separate from active human and machine time.

Acceptance includes cancellation of the production response-wait coordinator
and an actual child-process kill while that coordinator awaits a controlled
pending transport. After reopening SQLite, retry stays blocked until recovery;
the new attempt and its failure then replay correctly. This is a killed
validation-wait process, not a full deployed alert listener plus real SSH/Podman
power-loss test. No live VM or production service was started for this batch.

Combined review outcome, in priority order:

1. Must fix before continuing: fixed an acknowledgement consistency gap. The
   handoff digest alone was insufficient to detect conflicting candidate or
   incident/run journal links. Those bindings are now checked before pickup.
   A regression test failed before the fix and passes after it. No other new
   actionable blocker was found in this batch's transaction/replay paths.
2. Safe to defer within dormant staging: full deployed-responder/worker crash
   acceptance, detailed remote cleanup timing, candidate listing, and production
   enrollment. Cancelled/killed waits deliberately remain unmeasured. These are
   not evidence of full U9 release readiness.
3. Deliberate decisions: pickup is not approval; receipt confirmation is not
   current readiness; unknown cleanup stays unknown; old history is not
   backfilled; counters are not an uncertain-job gauge. No Restate, A2A,
   automatic GitHub publishing, or production activation is included.

Additional tests cover acknowledgement rollback, exact-artifact idempotency,
kernel-derived request identity, rejected forged identity fields, actual CLI
success/refusal exit codes, clock regression, and metrics without sensitive
labels. Earlier live-worker and Linux-root acceptance results above are historical
and were not rerun for this batch. No dependencies were added or upgraded.

Final batch validation: `env NEXTEST_TEST_THREADS=2 make ci` passed with local
socket access: 212 tests passed and two opt-in Tempo tests were skipped. Formatting,
Clippy, doctests, rustdoc, and cargo-deny passed. `git diff --check` passed. The
current branch still shares the latest `develop` base, `b947ebc`, checked before
preparing the single draft review PR. This batch does not claim full U9 completion.

## Research sources

- [serde_yaml_ng 0.10.0 API](https://docs.rs/serde_yaml_ng/latest/): latest release
  checked online before adding the parser dependency. Duplicate-key behavior is
  also covered by its upstream tests and our regression fixture.
- [GitHub pull-request API](https://docs.github.com/en/rest/pulls/pulls): draft and
  maintainer-modification controls do not replace credential role separation.
- [SQLite transactions](https://sqlite.org/lang_transaction.html): qualification
  and candidate commits use immediate transactions to serialize read/write decisions.
- [GitHub Actions security](https://docs.github.com/en/actions/reference/security/secure-use):
  untrusted head scripts must not run with privileged base credentials.
- [Podman run reference](https://docs.podman.io/en/latest/markdown/podman-run.1.html):
  container isolation flags and resource bounds used by the offline plan.
- [Git process controls](https://git-scm.com/docs/git): `GIT_NO_LAZY_FETCH`
  suppresses demand fetching; `GIT_ALLOW_PROTOCOL` overrides repository transport
  allowances so an empty allowlist denies all protocols.
- [Podman host information](https://docs.podman.io/en/latest/markdown/podman-info.1.html):
  runtime admission fields, including rootless security and cgroup controllers.
- [Podman 6.1.1 release](https://github.com/podman-container-tools/podman/releases/tag/v6.1.1)
  and [netavark 2.1.0 release](https://github.com/containers/netavark/releases/tag/v2.1.0):
  latest releases checked before building the Linux test runtime.
- [Alpine releases](https://www.alpinelinux.org/releases/): the containment-only
  fixture uses the inspected immutable digest of Alpine 3.24.1.
- [Podman removal](https://docs.podman.io/en/latest/markdown/podman-rm.1.html):
  forced cleanup and missing-container handling.
- [Podman container existence](https://docs.podman.io/en/latest/markdown/podman-container-exists.1.html):
  distinguish absence (1) from runtime/storage failure (125).
- [rusqlite open flags](https://docs.rs/rusqlite/0.40.2/rusqlite/struct.OpenFlags.html):
  no-follow database opens and connection threading constraints.
- [rustix effective UID](https://docs.rs/rustix/1.1.4/rustix/process/fn.geteuid.html):
  safe access to the kernel-derived service identity for SSH trust-path checks.

The local acceptance work did not change homelab services or credentials. The
later approved draft PR stages homelab source files only, as recorded above.
Earlier setup created the
private `bodhispace-xyz/ai-sre-u9-permission-canary` repository and disabled Actions.
It is unused by manual handoff and retained for possible future permission tests;
no deletion or new identity setup is required now.
