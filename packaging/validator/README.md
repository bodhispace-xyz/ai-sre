# Offline homelab validator

This image runs the homelab's real `make ci`: inventory rendering, OpenTofu
validation, Ansible syntax checks, Compose checks, and script tests. It does not
deploy services, qualify a rollback image, or publish a PR. The Rust validator
owns isolation, resource limits, cancellation, cleanup, and result binding.

## Build

Build only from reviewed packaging and a clean archive of the chosen homelab
base. Do not pass a working checkout containing local state or credentials as
the named build context. For example, with the homelab checkout path supplied:

```sh
validator_source=$(mktemp -d)
git -C "$HOMELAB_REPO" archive "$HOMELAB_BASE" | tar -xf - -C "$validator_source"
docker build --build-context "homelab=$validator_source" \
  -t ghcr.io/bodhispace-xyz/ai-sre-validator:u9-offline-local \
  packaging/validator
```

The build downloads public dependencies. It copies only provider configuration
and the committed provider lock from the homelab context. It does not run the
repository's Makefile or scripts during the build. The final image contains a
provider mirror and Ansible collections; validation has no download fallback.
Provider versions match the repository lock, not the latest provider release:
changing them would change the configuration being validated.

The tool versions were checked against upstream releases on September 9, 2026:
OpenTofu 1.12.6, Docker CLI 29.8.0, Compose 5.5.1, ansible-core 2.21.3,
PyYAML 6.0.3, community.docker 5.3.0 and community.general 13.4.0. Base images
use immutable digests. OS packages and Python/collection transitive dependencies
resolve at build time; their installed versions are recorded under
`/opt/ai-sre-validator`. This is not a byte-reproducible build. Review and enroll
the resulting image digest; a tag or a successful build is not enrollment.

OpenTofu's mirror build currently warns that the locked Proxmox provider's
signing key has expired. Do not suppress verification or update the repository's
provider lock as part of a one-field repair. Review this upstream supply-chain
warning before production enrollment.

## Run and acceptance

Production uses `SandboxPlan` and `RootlessValidator`, not `docker run` or a
model-authored command. The fixed entrypoint copies `/source` to a new
`/work/repository` and runs `make ci` with a cleared environment. Reusing scratch
fails. `/work` permits execution for checked-in scripts and provider binaries;
`/tmp` and `/dev/shm` remain non-executable. The source and image root stay
read-only. No Docker socket, credentials, SSH agent, journal, or host home is
mounted. Compose performs static configuration checks without a daemon.

Use the isolated Linux setup described in
[the containment fixture](../../tests/fixtures/u9-sandbox/README.md).
Preload the image, inspect its digest in Podman, and commit the clean homelab
archive into a disposable Git repository. Supply these test-only environment
variables to the Linux Cargo test:

- `U9_HOMELAB_REPOSITORY`: absolute path to that disposable repository.
- `U9_OFFLINE_IMAGE`: preloaded `ghcr.io/bodhispace-xyz/ai-sre-validator@sha256:…`.
- `U9_RUNTIME_DIGEST`: independently checked root-owned Podman binary digest.

```sh
cargo test --locked --lib linux_offline_homelab_checks_use_production_runner \
  -- --ignored --nocapture
```

Repeat with a separately committed repository containing
`infra/opentofu/lxc/invalid-fixture.tf`, whose content is
`This deliberately invalid fixture must fail offline validation.` Set
`U9_EXPECT_VALIDATION_FAILURE=1`. The test requires the `Invalid block definition`
diagnostic for that file; a missing image or unrelated failure does not pass.
Bounded diagnostics remain private to test builds. Both paths require container
and snapshot cleanup. These tests use synthetic deployment qualification, not production
health evidence. The root-only producer tests remain a separate hosted check;
they intentionally cannot all run inside this non-root validator.

## Durable worker ownership

Use `RootlessValidator::connect_with_state` for a persistent worker. Provision a
private, mode-0700 state directory on local storage before starting it. Give each
dedicated runtime one state directory; the owner lock prevents concurrent workers
using that directory. The older `connect` constructor has no crash journal.

The journal binds to Podman's resolved graphroot and runroot, storage driver,
canonical home and runtime paths, and the graphroot's device, inode, and owner.
Recovery checks that binding before accepting container absence or deleting
anything. Transient storage is rejected. Runroot inode changes across reboot are
allowed; graphroot replacement is not. Keep runtime configuration stable while
the worker runs. A pending journal without a binding requires operator review;
the worker will not guess which runtime originally owned it.

The worker commits launch intent before starting Podman. Recovery checks the
recorded name and random ownership label, persists the immutable container ID,
then removes that ID. It deletes a snapshot only when its path, owner, device,
and inode still match the recorded lease. Interrupted jobs never produce successful
validation receipts.

If a launch has no recorded container ID and no visible container, recovery blocks
new work. An old launcher might still create it later. Do not delete the journal
to bypass this check. Supervisor-assisted reconciliation of this uncertain state
is not implemented yet. Application shutdown must still await cleanup; aborting a
task is not a substitute. Authenticated remote transport and application wiring
remain separate work.

A confirmed OS spawn failure is different from an interrupted launch: no workload
process started. The worker removes that job's snapshot and clears its intent
synchronously, allowing another attempt. A started Podman process that exits with
an error still follows conservative recovery; its exit code alone does not prove
that no container was created.

The staged authenticated channel and application coordinator are described in
[SSH validation worker](WORKER.md). They do not activate a production worker.

## Sources

- [OpenTofu multi-stage images](https://opentofu.org/docs/intro/install/docker/)
- [OpenTofu provider installation configuration](https://opentofu.org/docs/cli/config/config-file/)
- [OpenTofu releases](https://github.com/opentofu/opentofu/releases)
- [Compose releases](https://github.com/docker/compose/releases)
- [Docker official image definitions](https://github.com/docker-library/docker)
- [ansible-core releases](https://pypi.org/project/ansible-core/)
- [PyYAML releases](https://pypi.org/project/PyYAML/)
- [community.docker releases](https://github.com/ansible-collections/community.docker/releases)
- [community.general releases](https://github.com/ansible-collections/community.general/releases)
