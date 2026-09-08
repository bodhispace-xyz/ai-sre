# U7 acceptance audit — 2026-09-08

U7 code and isolated backend acceptance checks are complete. This does not
declare the AI-SRE service deployed or grant mutation readiness. The final
review covers trace serialization, resource bounds, application integration,
loss accounting, report correlation, and failure isolation. This is an author
review and execution record, not an independent reviewer sign-off.

## Corrections

- OTLP uses fixed-width hexadecimal IDs, start/end timestamps, and service.name.
- Private event fields enforce controlled metadata. Correlation inputs are hashed;
  prompts, evidence bodies, credentials, and tokens are not exported.
- HTTP failures, partial rejection, malformed acknowledgements (including invalid
  partialSuccess types), oversized responses, and queue overflow count as loss.
  Redirects are disabled; HTTP time and response size are bounded.
- The authenticated metrics route reads live exporter loss accounting.
- Investigation spans describe the completed interval and terminal domain status.
- Standard base/signal endpoint precedence, configurable limits, disabled export,
  and report-page trace lookup IDs are covered by tests.
- CI identified RUSTSEC-2026-0258 in h2 0.4.15. The lockfile resolves h2 0.4.19.
  Cargo also moved four Windows dependency edges to the already present
  windows-sys 0.52.0; no other package versions changed.

Protocol reference: [OTLP specification](https://opentelemetry.io/docs/specs/otlp/).

## Acceptance evidence

- Real Rust OTLP export into disposable Tempo 3.0.3 was queried successfully.
  Sensitive canary inputs were absent. The trace remained queryable after a
  persistent-volume container restart.
- Disposable Grafana 13.1.1 returned datasource health OK and the trace through
  its datasource proxy.
- Accelerated retention changed trace lookup from HTTP 200 to 404 and logged
  actual block deletion. This exposed the need to configure both scheduler and
  backend-worker retention; production now sets both to 24 hours.
- Homelab [PR #486](https://github.com/bodhispace-xyz/bodhispace-homelab/pull/486)
  merged at revision 1f27c0b3b8661a888e0183e97322e3d989d5f26f.
  [Deployment run](https://github.com/bodhispace-xyz/bodhispace-homelab/actions/runs/34171442807)
  succeeded. Read-only checks in monitoring LXC 119 on pve2 confirmed readiness,
  no restarts, persistent storage ownership, loopback host bindings, Grafana
  connectivity, and successful Gatus health. Synthetic trace
  135694a9d3d8450bbafd70f2606ce4d0 was ingested and queried on the live backend.
  Live Tempo was not restarted or filled for testing.
- The actual AI-SRE binary accepts an authenticated synthetic alert, publishes
  its report, commits IncidentCompleted and the report to SQLite, and restores
  the same report after process restart with export disabled, unreachable,
  stalled, or returning HTTP 507. Export failures are observable in metrics.
  Paid providers are disabled and evidence is synthetic: this qualifies the
  deterministic incident path, not a live paid-provider investigation.
- The same durability probe passed against a disposable Tempo with a 64 KiB
  data tmpfs. Tempo logged "no space left on device" while completing a WAL block.
  The probe was repeated after that error was observed and passed.
  Backend acknowledgement can precede a failed flush, so this test deliberately
  does not demand an exporter loss increment for asynchronous backend loss.

Normal CI includes process-level failure tests. Real Tempo query and storage-full
probes remain explicit opt-in tests; they passed separately.
Final `make ci` passed formatting, strict Clippy, 138 tests (two opt-in tests
skipped), documentation, and cargo-deny. Duplicate dependency warnings are
non-fatal. `git diff --check` passed.

## Prioritized findings

### 1. Must fix before continuing

No remaining actionable code defects were identified in this review. Before
production AI-SRE-to-Tempo connectivity, define the actual AI-SRE deployment
inventory and allow only its required OTLP ingress. Tempo currently exposes host
ports on loopback, so a remote service cannot simply use the monitoring address.
This deployment prerequisite is not an implemented connection or a signed
mutation-readiness gate.

### 2. Safe to defer

- Restate, A2A, cross-agent propagation, and an OpenTelemetry collector.
- Additional spans for mutation workflows not yet implemented.
- Live paid-provider trace qualification during the service deployment rollout.

### 3. Deliberate design decisions to document

- SQLite is authoritative. Lost traces neither authorize nor block work.
- Export is bounded and best effort, without retries or guaranteed shutdown
  flush. Counters are process-local and cannot detect loss after backend ACK.
- Hashing enables lookup but does not anonymize guessable identifiers.
- Provider/outcome attributes describe the terminal result, not each fallback.
- Restart and disk exhaustion are tested on disposable backends; production
  verification uses non-destructive health and synthetic trace checks.
