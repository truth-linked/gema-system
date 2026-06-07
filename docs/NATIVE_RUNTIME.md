# Native Runtime Lane

Gema's native runtime is now a long-running host daemon, not a Docker-owned
session launcher. The daemon owns workspace volumes, lifecycle state, port
routing, metrics, snapshots, R2 hooks, agent sockets, and session hardening.

## Execution Path

- `gema-runtime hostd serve` listens on the host daemon socket.
- `scripts/gema-hostd.sh` is the root-safe control wrapper.
- `launch` starts `scripts/gema-substrate-run.sh` with a managed workspace volume.
- Public TCP and UDP exposure is handled by `gema-runtime hostd router`.
- Session state is written under `/var/lib/gema-system/sessions/<session_id>`.
- Workspace data is mounted from `/var/lib/gema-system/volumes/<session_id>.img`.

The launch interface is:

```bash
sudo scripts/gema-hostd.sh launch <session_id> <host_port|auto> <guest_port> [tcp|udp] \
  [--health-path <path>] \
  [--startup-timeout <seconds>] \
  [--service-timeout <seconds>] \
  [--health-timeout-ms <ms>] \
  [--restart-policy <never|on-failure|always>] \
  [--restart-max <count>] \
  [--profile <rootfs_profile>] \
  -- <command...>
```

## Readiness

Readiness is checked against the guest service path, not only the host router.

- Without `--health-path`, hostd waits for the guest TCP port to accept.
- With `--health-path`, hostd performs an HTTP probe and requires a 2xx or 3xx
  response.
- `--startup-timeout` bounds substrate and workload process appearance.
- `--service-timeout` bounds service readiness after a live workload appears.
- Both default to 120 seconds because first launches can spend real time in
  rootfs, network namespace, mount, and cgroup setup.
- If readiness fails before the startup timeout, hostd marks the session failed,
  releases public ports, and cleans preserved runtime state.

## Restart Policy

Restart policy is active runtime behavior.

- `never` disables restarts.
- `on-failure` restarts when the workload exits non-zero or disappears without a
  clean exit code.
- `always` restarts regardless of exit status unless a stop was explicitly
  requested.
- `--restart-max` bounds restart attempts.

Restart attempts preserve the session identity and reuse the original launch
request.

## Security Posture

The substrate applies the current hardening set before workload execution:

- uid/gid `1000`
- `no_new_privs`
- capability bounding, inheritable, and ambient sets dropped
- custom seccomp wrapper for dangerous host-level syscalls
- optional AppArmor profile wiring
- per-session cgroup tracking
- cgroup v2 eBPF device allowlist attached before workload entry

The seccomp filter currently denies high-risk syscalls such as mount, ptrace,
bpf, module loading, reboot, namespace switching, and related kernel control
paths. The device policy is generated from the default safe character devices
plus explicit GPU/device binds, then attached with `BPF_CGROUP_DEVICE`.

## Storage

Each session receives a mount-backed ext4 workspace image. Storage policy is
recorded per session and used by snapshot, restore, cleanup, and quota reporting
paths.

R2 commands exist for:

- configure
- status
- sync
- mount
- unmount

## Ports

Hostd owns port allocation and release. Allocations are recorded under the port
state store and audited in JSONL.

Supported exposure:

- TCP public routing
- UDP public routing
- private allocations
- automatic host port assignment
- explicit host port requests

## Images And Profiles

Image catalog, compatibility checks, verification, pull, unpack, cache, and
prune are in hostd. Optional cosign enforcement is fail-closed when
`GEMA_IMAGE_COSIGN_PUBLIC_KEY` is configured.

Image pull now attempts to promote the image into a managed rootfs profile:

- `skopeo` plus `umoci` is preferred when available.
- Buildah pull/from/mount plus `rsync -aHAX --numeric-ids` is used when that is
  the available daemonless backend.
- The promoted rootfs receives `.gema-rootfs-ready` and
  `etc/gema-image.json`.

Launch can select a managed rootfs profile with `--profile` or a managed rootfs
path with `--rootfs`.

## GPU State

GPU assignment metadata and device bind plumbing exist. Paid GPU readiness gates
fail closed when required NVIDIA/ROCm runtime, encoder, or R2 components are not
present.

This CPU host cannot validate real CUDA/ROCm execution, CDI behavior, MIG,
encoder behavior, or GPU isolation under load. Those checks must run on a GPU
node before the GPU lane is called production-ready.

## Logs

Session logs are bounded by:

- `GEMA_MAX_LOG_BYTES`, default `25 MiB`
- `GEMA_LOG_RETENTION_COUNT`, default `5`

`hostd logs` rotates oversized session `.log` files before returning tails.
Runtime events, lifecycle events, agent audit, port audit, and metrics are still
JSONL files. Structured indexing and cross-session retention policy remain open
work.

## Current Gaps

- GPU execution needs validation on real NVIDIA and ROCm hosts.
- OCI image promotion is implemented, but arbitrary images are only launchable
  when they contain the runtime entry tools Gema requires, such as `setpriv`.
- Log indexing and long-term retention policy are still basic.
- The local release gate is `sudo scripts/smoke-runtime.sh`. It validates
  launch, HTTP readiness, cgroup v2 eBPF attach and denial, restart recovery,
  UDP routing, image promotion, non-launchable image rejection, cleanup, BPF pin
  cleanup, cgroup cleanup, and port leak cleanup.
