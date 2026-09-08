# Tempo tracing

Tempo is an optional observation sink for the shadow responder. Configure only
the OTLP/HTTP `/v1/traces` endpoint and keep its queue bounded. AI-SRE exports
trace and span metadata such as phase, provider alias, duration, and stable
correlation IDs; prompts, tool results, log bodies, evidence text, secrets, and
approval tokens are never trace attributes.

If Tempo rejects or times out an export, the exporter drops events and increments
its loss counter. A backend may acknowledge before a later disk write fails;
that loss is visible only in backend monitoring, not the exporter counter.
Export never blocks intake, journal commits, provider
fallback, or recovery. The incident journal remains authoritative after Tempo
restart or retention expiry.

The exporter uses OTLP HTTP JSON. Set `OTEL_EXPORTER_OTLP_ENDPOINT` to a base
URL, such as `http://tempo:4318`; `/v1/traces` is appended to its path. The
trace-specific `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` overrides it and is used
as-is. Migrate deployments that previously put the full trace URL in the base
variable to the trace-specific variable. Missing or invalid endpoints disable
export without starting a worker. Credentials in URLs, query strings, fragments,
and non-HTTP schemes are rejected.

The optional application JSON `tracing` object accepts `queue_capacity`
(default 128; range 1–4096), `timeout_ms` (default 1000; range 1–10000), and
`max_response_bytes` (default 16384; range 1–65536). Invalid limits fail startup
configuration validation. Omitting the object preserves these defaults.
Redirects are disabled. Failed HTTP requests, rejected spans,
malformed acknowledgements, and queue overflow increment
`ai_sre_trace_dropped_total` on the authenticated metrics endpoint. No retry is
performed by this observation path.

The application emits a completed investigation span. Its incident page displays
the trace ID for copying into Grafana Explore's Tempo trace-ID query. Displaying
the ID does not imply that export succeeded. Correlation is the first
32 hexadecimal characters of SHA-256 of the incident identifier; span identity
is the first 16 hexadecimal characters of SHA-256 of `run_id:operation_name`.
These deterministic identifiers support correlation without exporting the raw
identifiers; hashing is not a confidentiality guarantee for guessable inputs.
The end timestamp is wall-clock time and the interval uses measured runtime
duration. Domain status supplies terminal provider labels `openai`, `gemini`,
`deepseek`, or `deterministic`, and outcomes `succeeded`, `baseline`, or
`exhausted`. Application errors use `failed` with no provider attribution.
This labels the terminal provider, not every fallback attempt.

Backend deployment and incident failure-isolation evidence are recorded in the
[acceptance audit](../reviews/2026-09-08-u7-acceptance-audit.md). Connecting a
deployed AI-SRE instance still requires its inventory identity and scoped ingress.

An opt-in real backend probe is available once disposable Tempo is running:

```sh
AI_SRE_TEST_OTLP=http://127.0.0.1:14318/v1/traces \
AI_SRE_TEST_TEMPO=http://127.0.0.1:13200 \
cargo test --test u7 real_tempo -- --ignored --nocapture
```

Use only a disposable backend: the probe writes a synthetic trace. It is skipped
by ordinary CI and has a bounded wait for trace availability. Homelab installation
and locally verified retention configuration are in
[PR #486](https://github.com/bodhispace-xyz/bodhispace-homelab/pull/486).

Ordinary CI also runs the actual service with synthetic evidence and no paid
providers, checking report publication, SQLite completion, and report recovery
after restart with export disabled, unreachable, stalled, or rejected.
The disk-full probe requires an operator-prepared disposable Tempo:

```sh
AI_SRE_TEST_FULL_OTLP=http://127.0.0.1:14320/v1/traces \
cargo test --test u7 actual_incident_survives_real_storage_full_tempo -- --ignored --nocapture
```

Verify actual storage-exhaustion errors in the disposable backend logs before
running it. The test proves incident durability, not the backend's disk state.
Never fill production storage for this test.
