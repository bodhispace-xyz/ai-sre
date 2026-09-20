# SSH validation worker

For responder-side inspection and explicit retry eligibility, see [local operator recovery](OPERATOR.md). This does not clear worker-side uncertain jobs.

This is a staged integration, not a deployed service. The responder uses a dedicated
SSH identity to invoke `ai-sre-validator-v1`. SSH authenticates both ends. The
remote account must force execution of the Rust `ai-sre-validator` binary; it must
not provide a shell, forwarding, PTY, user startup scripts, or an SSH agent.

The responder sends typed data: candidate/snapshot digests, base SHA, replacement
image, enrolled validation policy, a random request ID, and expiry. It cannot send
a command, repository path, patch, environment, or container arguments. The worker
captures the exact HEAD from its pre-provisioned read-only Git mirror and runs the
existing rootless validator. It receives no responder journal, OAuth data, GitHub
credential, or private SSH key.

## Deployment contract

- Install the worker binary and JSON configuration under root-owned, non-writable
  paths. The binary checks configuration path ownership and rejects symlinks.
- Set `AI_SRE_VALIDATOR_CONFIG` in the protected forced command. The worker requires
  `SSH_ORIGINAL_COMMAND=ai-sre-validator-v1`, as supplied by sshd. Do not accept
  client environment overrides. Provision a valid rootless `HOME`, runtime
  directory, user session, and cgroup delegation.
- Configure a dedicated authorized key with `restrict`. Also disable forwarding,
  tunnels, PTYs, user rc files, and user environment overrides in sshd. Use
  `ForceCommand` for the fixed worker executable. Keep other accounts and normal
  homelab SSH access separate.
- Provision one mode-0700 state directory for all worker invocations. It contains
  durable runtime binding, launch intent, ownership lock, and consumed request IDs.
- Independently enroll the same substantive image, Podman digest, and resource
  policy on the responder and worker. Neither learns policy from received data.
- Pin the host key out of band in the responder's dedicated known-hosts file.
  Mount it and the dedicated service key read-only. Do not reuse an operator key,
  personal `~/.ssh/config`, agent, or GitHub session.
  The client checks the complete path chain: files and directories must belong to
  root or the service's actual OS user, with no symlinks or group/world-writable
  entries. Root-owned sticky ancestors such as `/tmp` are allowed. This assumes
  root and other processes under the service identity are trusted.
- Keep both repository mirrors synchronized through a separate trusted read-only
  process. Validation never fetches. A missing commit or moved HEAD fails closed.
  Checking local HEAD is not proof of freshness against the upstream remote.

The worker configuration has `repository`, `runtime`, `state`, and `policy` fields.
`policy` uses `HandoffPolicy`: `max_validation_age_seconds`, `validator_image`,
`runtime_digest`, and `sandbox_limits`. Paths belong to deployment configuration,
not request data. Configuration and each protocol frame are limited to 16 KiB.

## Lifecycle and results

Each invocation handles one length-framed request. Request admission has a
10-second input deadline. Work expires after the enrolled container deadline plus
60 seconds; cleanup must succeed before any successful response. Request IDs are
consumed before work and remain consumed on failure. Duplicate IDs are rejected,
not rerun. The ledger stops admission at 4096 entries rather than discarding replay
protection. Reviewed retention/recovery tooling is still needed before production.

The client does not retry automatically. A timeout or broken SSH connection is an
uncertain result, not proof that the remote container stopped. The client kills
and joins its local SSH child on handled failure; the remote worker remains
deadline-bounded and reconciles durable launch state on its next invocation.
Aborting the caller's task is not an awaited cleanup guarantee. Explicit process
signal supervision and richer uncertain-result reconciliation remain open.

The application also reserves one durable attempt per candidate before SSH starts.
It stores the exact request, including its nonce and expiry. Retries and restarts
return `AttemptExists` instead of generating another remote workload for that
candidate. This conservative state includes a crash before dispatch. A received
bound receipt changes the attempt to `Validated`, but does not itself make an
artifact ready; current source and journal qualification checks still apply.
Attempt records expose audit data, not a way to deserialize trusted receipts.
There is no automatic reset or result replay. The optional local admin socket can
archive an expired attempt and reopen eligibility with an audited operator reason.
It does not dispatch validation or clear worker-side uncertainty. Follow
[the operator procedure](OPERATOR.md); do not edit database rows to bypass it.

A response becomes a `ValidationReceipt` only after SSH succeeds and the request
ID, candidate, snapshot, image, runtime, policy, output bounds, and time checks
match. No raw workload output is imported. The application coordinator then
recaptures local HEAD and calls the existing transactional handoff finalizer,
which rechecks journal qualification/evidence. The result is an operator-review
artifact; remote repository CI is still marked `not_run`. It grants no publication,
merge, or deployment authority.

## Integration entry points

- `SshValidator`: serialized authenticated client with explicit deployment settings.
- `serve_worker`: one request over an already authenticated stream.
- `ai-sre-validator`: the fixed SSH process entry point.
- `application::manual_repair::validate_manual_repair`: remote validation, local
  source recheck, and journaled handoff.
- `application::manual_repair::prepare_incident_repair`: reconstructs a candidate
  from current journal qualification, scoped evidence, and committed source.
- `application::manual_repair::run_incident_repair`: explicitly connects that
  preparation to remote validation and durable handoff. Restart reconstructs the
  candidate; it does not deserialize stored JSON into authority or reset an attempt.

The ignored Linux test
`incident_failure_restart_recovery_and_handoff_over_real_transports` exercises
this complete entry point through the real SSH worker and `ai-sre-admin`. Supply
`U9_HOMELAB_REPOSITORY`, `U9_OFFLINE_IMAGE`, `U9_RUNTIME_DIGEST`, and `U9_ADMIN_CLI`
with the same isolated fixture used above. It deliberately rejects the host key,
reopens the journal, proves implicit retry and early recovery fail, waits for the
real request deadline, then explicitly recovers and validates. It verifies handoff
delivery after reopen and rejection after qualification revocation. The default
request lifetime makes this test take more than three minutes. Run it explicitly,
not through a CI profile whose per-test timeout is shorter.

Qualification and incident evidence are synthetic in this test. "Restart" here
means reopening the journal, not a hard kill of the running responder. Neither a
live Alertmanager incident nor a production deployment is exercised.

The coordinator is not yet enabled by the incident loop or an operator endpoint.
Deployment provisioning, live receipt ingestion, current-readiness assessment, and remote
Git freshness remain separate acceptance work. No homelab deployment is changed
by building these entry points.

## Verification

Ordinary tests cover framing limits, expired admission, routing injection,
request replay across journal reopen, and response binding. The opt-in Linux
offline acceptance test supports `U9_WORKER_SSH=1` to exercise the application
coordinator through a real localhost SSH forced command and substantive image.
That fixture expects a dedicated `validator` account, port 22222, and disposable
key/known-hosts files under `/home/validator/u9-ssh-client`; `wrong_hosts` must
contain a different key. Never point this fixture at a production SSH service.

OpenSSH's [client configuration](https://man.openbsd.org/ssh_config) and
[authorized-key restrictions](https://man.openbsd.org/sshd) informed these controls.
[Upstream OpenSSH](https://www.openssh.org/releasenotes.html) was 10.5 when checked;
the isolated Fedora 44 test used its latest offered package, 10.2p1-14.fc44. This
test does not enroll that package or the local debug worker build for production.
