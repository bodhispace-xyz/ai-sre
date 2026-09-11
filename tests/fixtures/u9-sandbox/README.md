# U9 Linux acceptance fixtures

These fixtures test containment and filesystem provenance. They do not run
homelab CI, observe a deployment, or qualify a real rollback image. Never install
the synthetic receipt in a production inbox or select this image as the production
validator.

The tests are opt-in. Use an isolated Linux machine with no host-file sharing,
SSH-agent forwarding, service credentials, or production data. Run Cargo and
Podman as a non-root user.

## Containment

Provision a root-owned Podman binary at `/usr/local/bin/podman`, with rootless
UID/GID mappings, cgroup v2 CPU/memory/PID controllers, and seccomp. Install its
matching runtime helpers and configuration. Configured host mounts in any
`mounts.conf` cause admission to fail. The test never disables them itself.

On 2026-09-08, acceptance used Podman 6.1.1, netavark 2.1.0, and crun 1.28 in
an isolated Fedora 44 OrbStack machine. Podman 6 required its shipped storage
defaults: the older Fedora defaults forced rootful storage paths. Fedora also
supplied a default RHEL-secret mount; it was preserved as
`/usr/share/containers/mounts.conf.u9-disabled` in this test machine only.

Preload the base image, then build the fixture without network access:

```sh
podman pull docker.io/library/alpine@sha256:e7a1a92a5bfeee40966aea60f0796b0e7917cc35591542701834f03a68fa3d18
podman build --network=none --pull=never \
  --build-arg BASE_IMAGE=docker.io/library/alpine@sha256:e7a1a92a5bfeee40966aea60f0796b0e7917cc35591542701834f03a68fa3d18 \
  -t ghcr.io/bodhispace-xyz/ai-sre-validator:u9-containment-only \
  tests/fixtures/u9-sandbox
```

Inspect the resulting image digest and independently enroll the trusted runtime
binary digest. Set `U9_CONTAINMENT_IMAGE` to the local fixture's immutable
`ghcr.io/bodhispace-xyz/ai-sre-validator@sha256:…` reference and
`U9_RUNTIME_DIGEST` to the enrolled `sha256:…` binary identity. Then run:

```sh
cargo test --locked --lib linux_rootless_containment -- --ignored --nocapture
```

The actual Rust runner admits, launches, inspects, and removes the container.
The fixture checks source readability, denied writes, process privileges,
seccomp, network interfaces, cgroup limits, scratch sizes, and cleanup. The
test uses synthetic qualification data and cannot prove upstream deployment truth.

## Protected receipt inbox

As root, provision a dedicated directory under protected root-owned ancestors.
Copy `receipt.json` to `protected.json` with owner root and mode 0644. Create
separate copies for these deliberately unsafe cases:

- `writable.json`: mode 0666.
- `user-owned.json`: owned by the non-root test user.
- `symlink.json`: a symbolic link to the protected file.
- `hardlink.json`: a copy with an additional hard link; do not hard-link the good file.
- `writable-parent/receipt.json`: a root-owned file below a mode-0777 directory.

Set `U9_RECEIPT_FIXTURES` to this directory and run as the non-root test user:

```sh
cargo test --locked --test u9 linux_receipt_inbox -- --ignored --nocapture
```

The test accepts only the protected file. It does not create or change the inbox.
