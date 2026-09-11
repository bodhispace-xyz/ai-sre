# Local repair inspection and recovery

The admin CLI reads stored repair candidates, records explicit handoff pickup, and permits an explicit retry after an expired validation attempt. It does not run validation, approve a repair, create a PR, or deploy anything.

## Enable the local socket

Set `AI_SRE_ADMIN_SOCKET` to an absolute path in an existing service-owned directory. Set `AI_SRE_OPERATORS_GID` to the deployment-resolved numeric ID of `ai-sre-operators` to admit that group. Without a group ID, only root is admitted. Without a socket path, the listener is disabled.

The directory and its ancestors must be real directories owned by root or the service user. They must not allow group or world writes. A root-owned sticky directory is allowed above the socket directory, but not as its direct parent. Give operators directory traversal access without write access. The service must have permission to assign the configured group to its socket.

The socket uses mode `0660`. The server also checks the kernel peer UID and effective primary GID. Supplementary group membership alone does not qualify: use `sg ai-sre-operators` or run the CLI as root. The request cannot supply an operator identity. This uses [Tokio's Unix peer credentials](https://docs.rs/tokio/latest/tokio/net/struct.UnixStream.html#method.peer_cred).

The listener refuses to replace an existing path. Normal shutdown removes only the socket inode it created. After an unclean exit, verify that the old process is gone before removing its stale socket.

## Inspect a candidate

```sh
ai-sre-admin inspect /run/ai-sre/admin.sock CANDIDATE_DIGEST
```

The JSON response includes the original candidate JSON, its patch and proposed PR description, the active attempt, the expected request digest, and the latest 100 recovery records. Older recovery records remain in SQLite. `artifact.handoff_json` contains the most recently stored handoff for that candidate, if one exists. It includes the bound validation result and suggested PR text. This is historical data, not a fresh readiness decision; recovery or later revocation does not erase it.

`Uncertain` means dispatch was reserved but no receipt was stored. `Validated` means a bound receipt was stored; it does not mean the candidate is currently ready. Every response says `readiness: not_assessed`. A recovered attempt appears in history rather than as an active attempt. Reading stored data does not restore a sealed validation receipt.

`cleanup_status` is `confirmed_by_receipt` only when the active attempt has a stored authenticated success receipt. Otherwise it is `unknown`, including after cancellation, process loss, or recovery. An SSH error does not prove remote cleanup failed or succeeded. Historical handoffs may still contain older receipts.

## Acknowledge handoff pickup

Inspection returns `handoff_digest` for the exact stored handoff and its existing `acknowledgement`, if any. After taking responsibility for reviewing that artifact, use:

```sh
ai-sre-admin acknowledge /run/ai-sre/admin.sock HANDOFF_DIGEST
```

This records pickup, not repair approval. It grants no publication or deployment authority and does not assess current readiness. Reading an artifact does not acknowledge it. Repeating acknowledgement for the same handoff returns confirmation without adding another event or replacing the original operator/time. An unknown handoff is refused; the CLI exits unsuccessfully unless the reply explicitly confirms acknowledgement.

The journal measures wall-clock time from durable handoff creation to explicit pickup. This includes downtime; it is not active human effort or notification-delivery latency. If the server clock regresses, pickup is still recorded but its duration is unknown. Forward clock changes can affect this wall-clock measure. Inspection after a lost reply shows whether pickup committed.

## Permit deliberate revalidation

First inspect the candidate and investigate the remote worker. Use the exact `expected_request_digest` from inspection:

```sh
ai-sre-admin recover /run/ai-sre/admin.sock CANDIDATE_DIGEST EXPECTED_REQUEST_DIGEST 'Worker inspected; requesting fresh validation'
```

The original request deadline must have passed. The server archives the request, any receipt, kernel UID/GID, reason, and server timestamp in one SQLite transaction before removing the active-attempt row. It does not delete attempt history. A stale recovery request cannot clear a newer attempt. An archived request cannot be reserved again.

Use reasons without secrets, raw logs, or credentials. Reasons are limited to 512 bytes and cannot contain control characters.

Recovery only reopens eligibility. A later call through the validation coordinator must create a fresh request and pass the worker's ownership recovery, source, qualification, evidence, policy, and handoff freshness checks. Recovery does not prove remote cleanup. An uncertain worker job can still block new work. The live incident-to-validation trigger remains unimplemented.

If the client loses its reply, inspect history before retrying. A lost reply does not undo a committed recovery. Late validation responses must match the exact active request before they can update it.

The CLI exits unsuccessfully if recovery is declined or its response does not confirm success. It still prints the received JSON for inspection. Exit code zero for `inspect` means the read completed, not that a candidate is ready.

## Operational limits

Socket authentication, frame reads, and reply writes run outside the incident worker. At most four client tasks and four queued commands exist. Only decoded, authenticated commands reach the journal worker, which executes database operations serially. This avoids both slow-client waits in incident processing and competing journal writers. Dropping the service requests cancellation of its socket tasks; [Tokio's task set cancels its children on drop](https://docs.rs/tokio/latest/tokio/task/struct.JoinSet.html). Cleanup is asynchronous, not an awaited guarantee after abrupt process exit. Recovery audits live in `manual_validation_recoveries`; new accepted recoveries also commit scoped incident facts and aggregate counters in the same transaction.

Successful and failed response waits use separate monotonic duration sums and measured-sample counts. Cancelled or killed waits remain unknown, with no invented duration. These measures exclude snapshot preparation, journal writes, and operator wait. Use measured-sample counts for averages. No metrics use incident, artifact, request, or operator labels. Legacy rows are not backfilled. The journal's new event variants require a compatible reader when downgrading binaries.

Each connection accepts one JSON frame: a four-byte big-endian length followed by at most 4096 bytes. Read, queue/reply-wait, and write deadlines are five seconds each. Responses are limited to 4 MiB. The worker checks the queue deadline before executing a command, so an expired queued recovery cannot mutate state later. The CLI waits at most 15 seconds; a timeout or lost reply after commit still requires inspection. There is no network admin endpoint and no model tool for recovery.

This slice does not recover worker-side uncertain containers, prune replay ledgers, list all candidates, or assess current publish readiness. Those remain separate work items. Do not enable this listener in production before review and deployment-specific permission checks.

## Linux identity acceptance

The ignored `linux_cross_user_cli_enforces_kernel_identity_and_audits_recovery` library test requires a disposable Linux environment, root, `/usr/bin/setpriv`, and `U9_ADMIN_CLI` pointing to the built admin binary. It uses UID/GID 501 for the operator and 65534 for a non-operator, matching the existing validator/nobody accounts in the test VM. It creates no accounts.

The test runs the actual CLI through the queued server. It checks filesystem rejection, kernel rejection of supplementary-only membership, root inspection, operator recovery, stale replay exit status, and the recorded operator identity. It removes its temporary binary, socket, and journal afterward. Run this test explicitly; normal CI does not claim this privileged acceptance merely because other tests pass.
