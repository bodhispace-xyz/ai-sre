# Deep review follow-ups — 2026-08-11

The U0 review fixes are intentionally separate from the next reasoning/tool
slice. This document records what is complete, what remains, and the gate that
must be met before enabling model-directed context queries in a live run.

## Completed in U0 review fix

- GCX query and concurrency limits are configurable through `AppConfig`.
- GCX uses literal argv, permits valid LogQL operators/backticks, and rejects
  empty or obviously unbounded selectors.
- GCX output failures clean up the child process before returning.
- Evidence queries consume the configured per-incident budget.
- Evidence is committed incrementally, so an early successful query survives a
  later query failure.
- Deterministic reports preserve the normalized alert name.
- Rig requests advertise the read-only `query_logs` and `query_metrics` tools.
- Prompts state available tools, remaining evidence allowance, and prohibited
  capabilities.

## Prioritized outstanding ledger

### 1. Must fix before enabling live model-directed queries

The provider boundary currently accepts one completion and normalizes its final
JSON report. It does not yet execute a tool call and resume the same reasoning
run. Before enabling the tools in production, implement this finite protocol:

1. receive a provider response;
2. validate the tool name and typed arguments;
3. reserve the incident query budget and wall-clock allowance;
4. execute only the corresponding read-only context capability;
5. commit a typed result envelope (success, denied, truncated, stale, or
   exhausted) to the evidence board and journal;
6. send the result back to the same provider run;
7. stop at a configured tool-turn limit and require a final validated report.

The core must own the protocol and budgets. Rig remains an adapter detail;
`gcx` remains a replaceable read-only implementation. No shell, arbitrary
subcommand, datasource selection, credentials, or mutation tool may be added.

### 2. Safe to defer until the tool-loop slice

- Move `ReadOnlyRequest` and result-envelope types from the Grafana adapter into
  the provider-neutral context module.
- Add explicit query range, step, line, sample, matcher, and cardinality
  policies once the supported `gcx` release and response shape are pinned.
- Add process tests proving immediate kill/reap when either bounded stream
  overflows, rather than only eventual timeout cleanup.
- Add concurrency-permit and partial-result integration tests at the
  investigation boundary.

### 3. Deliberate design decisions to retain

- The current fixed two-query seed pack remains deterministic enrichment; it is
  not the model tool loop.
- Provider fallback restarts a complete reasoning run over immutable evidence;
  providers do not share a mutable conversation.
- Tool results are evidence records, not authoritative state transitions.
- Socket-binding failures in the local sandbox are environment limitations, not
  product behavior; CI must run the transport tests in a network-capable job.

## Exit criteria for the next slice

- Outside-in tests cover at least one tool request, one denied request, budget
  exhaustion, timeout, truncated output, and resume-to-final-report flow.
- The journal records tool name, provider, query digest, result class, elapsed
  time, bytes, and budget before/after values without secrets.
- OpenAI, Gemini, and DeepSeek adapters expose the same provider-neutral loop
  contract, with provider-specific tool serialization isolated at the edge.
- The deterministic baseline still works when all model/tool attempts fail.
