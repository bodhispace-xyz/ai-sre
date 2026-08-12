# AI-SRE shadow deployment

The first deployment is a dedicated, unprivileged LXC with `mode=shadow`.
Shadow mode can investigate, persist a report, and notify through ntfy. It
cannot load an action policy, approval executor, GitHub credential, or broad
host mount. A missing, expired, revoked, or unverified Gate A manifest keeps
the service in shadow mode.

Deploy the digest-pinned image through the homelab GitOps repository. Project
static secrets as service-owned `0400` files, keep the OpenAI OAuth cache in a
backup-excluded `0700` directory, and verify that the image health endpoint and
the synthetic replay harness succeed before enabling real Alertmanager input.

Record the exact image digest, policy/config versions, provider admission
results, corpus digest, and quarantine state in the deployment journal.
