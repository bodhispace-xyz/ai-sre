# Incident-to-manual-repair dispatch

`AI_SRE_MANUAL_REPAIR_CONFIG` enables the incident integration. It defaults to
disabled. Its value is an absolute path to a Linux root-owned JSON file, with
protected ancestors and no group/world writes, links, or special files. The
configuration limit is 16 KiB. Invalid enrollment fails startup.

The configuration fields are:

| Field | Meaning |
| --- | --- |
| `schema` | `ai-sre/manual-repair-config/v1` |
| `repository` | Absolute path to the responder's read-only Git mirror |
| `checkpoint` | Absolute path to the trusted delivery checkpoint |
| `inbox` | Absolute path to the checkpoint's protected receipt directory |
| `max_checkpoint_age_seconds` | Maximum checkpoint lifetime, 1–300 seconds |
| `max_inbox_entries` | Scan bound, 1–4096 entries including temporary files |
| `max_inbox_bytes` | Scan bound, 1–67108864 encoded bytes |
| `alert_name` | Exact operator-enrolled alert name for the pilot image correction |
| `worker` | `host`, `user`, `port`, `identity_file`, and `known_hosts` from [worker enrollment](WORKER.md) |
| `policy` | `max_validation_age_seconds`, immutable `validator_image`, `runtime_digest`, and `sandbox_limits` |

Only a firing alert with `service=it-tools` and the enrolled alert name can
select this path. The root-owned checkpoint selects the qualified deployment.
Model text cannot select an image, command, worker, path, or deployment identity.
Enrollment must describe an image-correction alert; a generic availability alert
does not by itself establish that replacing the image is the right repair.

## Trusted source and delivery checkpoint

The trusted synchronizer owns upstream freshness. It observes the protected
homelab branch through its own read-only access, updates the responder and worker
mirrors to that exact commit, and transfers the producer's complete receipt set.
It must not use the AI-SRE process or its model tools for these writes. The
application does not read the operator's personal GitHub session.

The root-owned checkpoint has this schema:

```json
{
  "schema": "ai-sre/repair-checkpoint/v1",
  "repository": "bodhispace-xyz/bodhispace-homelab",
  "base": "FULL_UPSTREAM_COMMIT",
  "deployment_id": "PROTECTED_COMPLETION_ID",
  "observed_at": 0,
  "expires_at": 0,
  "receipt_digests": ["sha256:CANONICAL_RECEIPT_DIGEST"]
}
```

These placeholders intentionally cannot pass admission. The producer computes
digests over canonical `ReceiptWire` JSON, not file bytes. A shared golden receipt
tests Python/Rust serialization agreement. Every final receipt, including
revocations for other deployments, must appear exactly once. Missing, extra,
duplicate, malformed, mutable, or changed evidence rejects admission.

The homelab producer provides `checkpoint DEPLOYMENT_ID BASE OBSERVED_AT
--ttl-seconds SECONDS`, `export`, and `deliver DESTINATION`. Only trusted
orchestration may call them. `checkpoint` accepts the orchestrator's upstream
observation; it does not fetch or authenticate Git itself. Repeating it with an
old observation cannot renew its expiry. `export` checks the complete producer
set. `deliver` accepts a bounded bundle on stdin from an already authenticated
transfer and publishes immutable receipts before atomically replacing
`DESTINATION/checkpoint.json`. The destination and its `receipts` directory must
already have protected ownership. No HTTP import endpoint is provided.

Delivery keeps old receipts. It cannot silently discard a revocation to accept an
older or compacted producer set. An interrupted transfer leaves no new complete
checkpoint. The transport limit is 1 MiB and 256 receipts; larger histories require
reviewed retention work. Empty or stale delivery is not readiness.

Freshness is bounded by checkpoint expiry, not instantaneous. A revocation or
upstream update immediately after an observation can remain unseen until the
next transfer or expiry. Pick the lifetime and polling cadence together. Do not
refresh `observed_at` at the destination or publish a checkpoint merely because
its local directory is readable.

## Dispatch, recovery, and delivery

The application imports the complete checkpoint set, prepares a candidate from
durable incident evidence, then checks the checkpoint again before dispatch.
The existing coordinator reserves the attempt before SSH. After validation it
recaptures Git source, reloads the checkpoint, imports new receipts/revocations,
and checks expiry before transactional handoff. Changed base or deployment
selection rejects the result. Qualification and evidence are checked again by
the journal finalizer.

The incident loop permits one automatic validation attempt per incident,
including across new reasoning runs and restart. Failed or interrupted attempts
need explicit operator inspection and recovery. Recovery does not make the
incident loop automatically redispatch that incident. The staged coordinator
remains available for explicit revalidation by a trusted caller.

The durable incident report, web page, and ntfy message include the candidate
digest and result. Operators retrieve the artifact through `ai-sre-admin inspect`.
A failed admission leaves a recommendation; it does not mark a candidate ready.
Delivery is not publication, approval, deployment, or proof of incident recovery.
Historical inspection still reports `readiness: not_assessed`.

## Activation boundary

Local and synthetic Linux acceptance do not enroll production. Before enabling
this integration, review the exact alert selection, trusted source observation
and transfer job, producer begin/complete/revoke hooks, worker identity and host
key, validator digest, resource limits, and promotion requirements. Verify a real
producer-to-responder transfer and rejection after revocation. No production
credentials, hooks, host, or checkpoint are created by setting the environment
variable alone.
