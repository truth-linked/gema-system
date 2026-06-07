# Gema Host Daemon

`gema-hostd.sh` starts the Rust host daemon through `gema-runtime hostd`. It is
the long-running manager for the native substrate.

It is intentionally small:

- stays alive on the host
- accepts structured commands through `/run/gema-system/hostd.sock`
- launches substrate sessions
- wires TCP and UDP port maps into session network namespaces through the Rust router
- delegates cleanup to `gema-substrate-run.sh`
- persists lifecycle state and lifecycle events per session
- records security, image, GPU, storage, service, and recovery posture

## Systemd Hardening

The repo ships a hardened service template at:

```bash
/root/gema-system/scripts/gema-hostd.service
```

It is intentionally not a fully unprivileged service. Hostd must retain the
kernel controls required to create network namespaces, veth links, cgroups,
cgroup-device eBPF programs, overlay and workspace mounts, FUSE mounts, and
CRIU restore state. The unit narrows the daemon with a strict filesystem view,
private tmp, a bounded capability set, restricted address families, kernel log
and module protection, private keyring, and root-owned runtime/state
directories while leaving mount, cgroup, namespace, BPF, and network control
available.

Build the release binary before enabling the unit:

```bash
cargo build --release
sudo cp /root/gema-system/scripts/gema-hostd.service /etc/systemd/system/gema-hostd.service
sudo systemctl daemon-reload
sudo systemctl enable --now gema-hostd
```

## Start

```bash
sudo /root/gema-system/scripts/gema-hostd.sh start
```

## Launch A Service

```bash
sudo /root/gema-system/scripts/gema-hostd.sh launch web-1 18080 8000 -- \
  /bin/bash -lc 'python3 -m http.server 8000 --bind "$GEMA_GUEST_IP"'
```

Then open:

```bash
curl http://127.0.0.1:18080/
```

UDP services use the same launch path with an explicit protocol:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh launch udp-1 18081 9000 udp -- \
  python3 -c 'import os,socket; s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM); s.bind((os.environ["GEMA_GUEST_IP"],9000)); exec("while True:\n data,addr=s.recvfrom(65535)\n s.sendto(data,addr)")'
```

## Cleanup

```bash
sudo /root/gema-system/scripts/gema-hostd.sh cleanup web-1
```

## Lifecycle State Machine

Each session stores:

```text
/var/lib/gema-system/sessions/<session>/lifecycle.json
/var/lib/gema-system/sessions/<session>/lifecycle.events.jsonl
```

Allowed states:

```text
queued -> preparing -> starting -> ready -> running
running -> recovering -> running
running -> stopping -> stopped
starting -> failed
running -> failed
```

`inspect` returns the current lifecycle document. `logs` can stream runtime logs:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh inspect web-1
sudo /root/gema-system/scripts/gema-hostd.sh logs web-1 100
sudo /root/gema-system/scripts/gema-hostd.sh logs web-1 -f
```

Daemon restart is represented as:

```text
running -> recovering -> running
```

Normal stop preserves the lifecycle document and records:

```text
running -> stopping -> stopped
```

Failed startup preserves the lifecycle document and records:

```text
starting -> failed
```

When a launch request includes `health_path`, hostd probes the guest service
directly inside the session network path before promoting the session to ready.
The host router accepting a socket is not enough. If the health probe does not
pass before `startup_timeout_seconds`, the launch is marked failed and the port
allocation is released.

Restart policy is active for `restart_policy=always` and
`restart_policy=on-failure`. Hostd records restart attempts in `hostd.json`,
cleans the previous namespace/router state, remounts the durable workspace
volume, and relaunches the original request until `restart_max_attempts` is
reached.

## Runtime Event Ledger

Each session also stores an append-only runtime ledger:

```text
/var/lib/gema-system/sessions/<session>/runtime.events.jsonl
```

The ledger records operational events such as:

```text
session_launched
port_mapped
snapshot_started
snapshot_completed
snapshot_failed
gpu_attached
process_exited
recovery_attempted
agent_action
user_command
terminal_command
session_stopped
```

Manual event hooks:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh snapshot web-1 started '{"snapshot_id":"snap-1"}'
sudo /root/gema-system/scripts/gema-hostd.sh snapshot web-1 completed '{"snapshot_id":"snap-1","bytes":1234}'
sudo /root/gema-system/scripts/gema-hostd.sh snapshot web-1 failed '{"snapshot_id":"snap-2","error":"timeout"}'
sudo /root/gema-system/scripts/gema-hostd.sh gpu-attached web-1 '{"vendor":"nvidia","device":"gpu0"}'
sudo /root/gema-system/scripts/gema-hostd.sh agent-action web-1 inspect_desktop '{"tool":"desktop_screenshot"}'
sudo /root/gema-system/scripts/gema-hostd.sh user-command web-1 echo hello
sudo /root/gema-system/scripts/gema-hostd.sh terminal-command web-1 bash -lc 'echo terminal'
sudo /root/gema-system/scripts/gema-hostd.sh event web-1 recovery_attempted '{"reason":"manual"}' system warning
```

Every line is JSON and includes:

```text
id
session_id
type
actor
severity
details
at
```

## Execution Fabric

Hostd exposes the control plane for persistent runtime plus detachable
accelerator lanes.

Create or refresh the durable execution contract:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh execution-contract ml-1 \
  '{"command":["python","train.py"],"cuda_lane":"cu121","require_gpu":true}'
```

Attach a compatible accelerator lane:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh accelerator-attach ml-1 \
  '{"cuda_lane":"cu121","require_gpu":true}'
```

Read the current accelerator state:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh accelerator-status ml-1
```

Detach at a checkpoint boundary:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh accelerator-detach ml-1 drain
```

The files are stored per session:

```text
/var/lib/gema-system/sessions/<session>/execution_contract.json
/var/lib/gema-system/sessions/<session>/accelerator_state.json
/var/lib/gema-system/sessions/<session>/cuda_proxy_state.json
```

They are also mirrored for workers and agents:

```text
/var/lib/gema-system/workspaces/<session>/workspace/.gema/fabric/execution_contract.json
/var/lib/gema-system/workspaces/<session>/workspace/.gema/fabric/accelerator_state.json
/var/lib/gema-system/workspaces/<session>/workspace/.gema/fabric/cuda_proxy_state.json
/var/lib/gema-system/workspaces/<session>/agent/fabric/execution_contract.json
/var/lib/gema-system/workspaces/<session>/agent/fabric/accelerator_state.json
/var/lib/gema-system/workspaces/<session>/agent/fabric/cuda_proxy_state.json
```

Attach is truthful. If no GPU assignment exists and `require_gpu` is true,
hostd records `blocked_no_gpu_assignment` and emits a warning event. If
`require_gpu` is false, hostd records `waiting_for_accelerator` and keeps the
session ready for a later GPU worker. If a GPU is assigned, hostd records
`attached` and the agent can route accelerated work to the execution twin.

The execution contract now states the hard boundary explicitly. GVPhase1 is the
primary CUDA API remoting data plane for terminal-visible remote GPU. The fabric
bridge is control plane only: provisioning, SSH tunnel setup, file transfer,
heartbeats, logs, checkpoints, and recovery coordination. The accelerator worker
is coarse task offload fallback.

Hostd exposes this with:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh cuda-proxy-status <session_id>
sudo /root/gema-system/scripts/gema-hostd.sh cuda-proxy-contract <session_id> '{"validation":{"real_gpu_probe_passed":true}}'
```

Gema can claim the twin architecture only when `cuda_proxy.ready=true` and the
real GPU probe has passed on the attached backend.

GPU workers consume the mirrored contract with:

```bash
gema-accelerator-worker --workspace /path/to/workspace --agent-dir /path/to/agent
```

The worker validates the execution contract and recovery policy, selects the
requested CUDA lane through `gema-cuda-select` when present, runs the workload
through `gema-ml-bridge` when available, writes heartbeats into
`.gema/fabric/accelerator_state.json`, and records an
`accelerator_worker.events.jsonl` ledger in the workspace.

## Metrics Collector

The daemon samples active sessions in the background and supports on-demand
collection:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh metrics web-1
```

Per-session metrics are stored at:

```text
/var/lib/gema-system/sessions/<session>/metrics.latest.json
/var/lib/gema-system/sessions/<session>/metrics.samples.jsonl
```

Current fields:

```text
cpu.usage_usec
cpu.user_usec
cpu.system_usec
cpu.percent
ram.current_bytes
ram.peak_bytes
ram.max_bytes
disk.workspace_bytes
disk.filesystem_total_bytes
disk.filesystem_used_bytes
disk.filesystem_free_bytes
network.interface
network.rx_bytes
network.tx_bytes
network.rx_packets
network.tx_packets
processes.count
processes.max
processes.pids
uptime_seconds
gpu.utilization_percent
gpu.vram_used_bytes
gpu.vram_total_bytes
gpu.temperature_c
gpu.power_watts
```

GPU fields are reserved and return `null` on CPU-only hosts.

## Security Posture

Every native session now enters the workload as uid/gid `1000`, with
`no_new_privs` enabled, seccomp filter mode enabled, and the capability
bounding, inheritable, effective, and ambient sets dropped. Session metadata
records the active posture at:

```text
/var/lib/gema-system/sessions/<session>/security.json
```

Hostd also records an explicit isolation lane in `hostd.json` and
`security.json`. This is the contract between product launch, host gates, and
runtime execution:

```text
standard        Gema native Linux isolation for normal CPU workspaces.
desktop-native  Native display/audio/input lane for interactive desktops.
gpu-native      Near-native GPU lane for CUDA/ROCm, NVENC, Sunshine, and ML.
secure          gVisor-required lane for CPU-only untrusted workloads.
```

The `gpu-native` and `desktop-native` lanes keep raw performance by staying on
the real Linux kernel while enforcing namespaces, cgroups, seccomp-BPF,
capability drop, no-new-privs, rootless workload entry, device allowlists,
mount-backed workspace quotas, and append-only audit ledgers. The `secure`
lane fails closed unless `runsc` is installed, so Gema cannot accidentally
market a gVisor sandbox on a host that does not have it.

The host probe fails paid GPU placement when required isolation primitives are
missing:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh host-probe free
sudo /root/gema-system/scripts/gema-hostd.sh host-probe paid-gpu nvidia
```

The device policy is deliberately narrow: the rootfs keeps only a minimal
static `/dev`, and GPU devices are added only through explicit `/dev` bind
allowlists generated by `assign-gpu`. On cgroup v2, device filtering requires
the eBPF device controller path; the probe reports that mode so hosts cannot
silently pretend to have Docker-style `devices.allow` files.

The seccomp executor denies mount, namespace re-entry, kernel module loading,
eBPF loading, perf tracing, raw reboot, swap, and other host-control syscalls
while allowing normal user workloads to run.

## Image Management

Hostd keeps a rootfs catalog and can verify, pull, cache, and prune OCI images:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh images
sudo /root/gema-system/scripts/gema-hostd.sh image-compat any cpu
sudo /root/gema-system/scripts/gema-hostd.sh image-verify docker.io/library/busybox:latest
sudo /root/gema-system/scripts/gema-hostd.sh image-pull docker.io/library/busybox:latest
sudo /root/gema-system/scripts/gema-hostd.sh image-prune
```

If `GEMA_IMAGE_COSIGN_PUBLIC_KEY` is set, `image-verify` and `image-pull`
require `cosign verify` to pass before a pulled image is recorded in the cache
index.

## GPU Runtime Gate

Paid GPU probe mode now checks the driver CLI, CDI metadata, encoder support,
R2 readiness, and explicit device bind posture. CPU-only hosts are expected to
fail paid GPU probe mode, which prevents accidental GPU placement on a node
that cannot run the workload.

## Storage Quota Enforcement

Hostd tracks workspace and snapshot storage per session:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh storage web-1
sudo /root/gema-system/scripts/gema-hostd.sh set-storage-quota web-1 32212254720 2
sudo /root/gema-system/scripts/gema-hostd.sh mount-storage web-1 32212254720
sudo /root/gema-system/scripts/gema-hostd.sh unmount-storage web-1
sudo /root/gema-system/scripts/gema-hostd.sh cleanup-storage web-1
```

Per-session storage state is stored at:

```text
/var/lib/gema-system/sessions/<session>/storage_policy.json
/var/lib/gema-system/sessions/<session>/storage.latest.json
/var/lib/gema-system/sessions/<session>/storage.samples.jsonl
/var/lib/gema-system/sessions/<session>/snapshots.json
/var/lib/gema-system/sessions/<session>/storage_volume.json
```

Workspace storage is now hard-backed by a per-session ext4 filesystem image:

```text
/var/lib/gema-system/volumes/<session>/workspace.ext4
```

At launch, hostd creates or mounts that image at:

```text
/var/lib/gema-system/workspaces/<session>/workspace
```

The image size is the session quota. Inside the session this behaves like a
real finite disk: when it fills, normal writes fail at the filesystem layer.
This is the production hard-stop layer for hosts that do not have project
quotas mounted.

Tracked fields:

```text
workspace_bytes
snapshot_bytes
total_bytes
file_count
quota_bytes
available_bytes
percent_used
quota_exceeded
write_probe
cleanup_policy
```

Snapshot events now record snapshot size. If a snapshot event supplies
`snapshot_bytes`, `bytes`, `size_bytes`, `path`, or `snapshot_path`, hostd
stores the size in `snapshots.json` and includes it in quota accounting.

When quota is exceeded, hostd records `storage_quota_exceeded` and
`storage_write_failed` events. On legacy bind-mounted workspaces it also writes
a marker and marks the workspace root read-only as a host-side guard.

Project quota support is detected in host probes. On this host, `/var/lib` is
on ext4 without `prjquota`, so Gema uses mount-backed ext4 workspace images.

Cleanup policy currently supports:

```text
delete_tmp
delete_cache
delete_old_snapshots
snapshot_retention_count
```

## Host Probe Gates

Before paid GPU work, hostd can run launch-blocking host gates:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh host-probe free
sudo /root/gema-system/scripts/gema-hostd.sh host-probe paid-gpu nvidia true
sudo /root/gema-system/scripts/probe-host.sh paid-gpu nvidia true
```

The latest probe is stored at:

```text
/var/lib/gema-system/host_probe.latest.json
```

Probe gates:

```text
kernel_cgroup
filesystem
nvidia_gpu or amd_gpu
encoder
port_range
r2_credentials
daemon_health
```

Free CPU launches treat GPU, encoder, and R2 failures as warnings. Paid GPU
launches require GPU, encoder, R2, kernel/cgroup, filesystem, port range, and
daemon health to pass.

Launch requests that set `paid_gpu=true`, `gpu_vendor=nvidia`, `gpu_vendor=amd`,
or `mode=paid-gpu` are blocked if a required gate fails. The error names the
blocking gates.

## Snapshots, R2, Agent API, And GPU Assignment

Snapshot commands:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh snapshot-create web-1
sudo /root/gema-system/scripts/gema-hostd.sh snapshot-restore web-1 [snapshot_id]
sudo /root/gema-system/scripts/gema-hostd.sh process-checkpoint-create web-1 [checkpoint_id] [pid]
sudo /root/gema-system/scripts/gema-hostd.sh process-checkpoint-restore web-1 [checkpoint_id] [terminate-existing|allow-live]
sudo /root/gema-system/scripts/gema-hostd.sh process-checkpoints web-1
sudo /root/gema-system/scripts/gema-hostd.sh recovery-plan web-1
```

Snapshots write local tar archives under:

```text
/var/lib/gema-system/workspaces/<session>/snapshots
/var/lib/gema-system/sessions/<session>/snapshots.json
/var/lib/gema-system/sessions/<session>/recovery_plan.json
```

Each completed snapshot now has three durable layers:

```text
<snapshot>.tar
<snapshot>.session.json
<snapshot>.runtime-diff.tar
```

`<snapshot>.tar` captures the workspace filesystem. `<snapshot>.session.json`
captures the launch and recovery state: command, selected rootfs, runtime
rootfs, lifecycle, ports, network, storage policy, storage volume, GPU
assignment, R2 state, agent socket config, security posture, and process
supervisor state. `<snapshot>.runtime-diff.tar` captures the per-session rootfs
overlay upperdir, which is where runtime changes outside the workspace land.

Layer 4 is process checkpoint/restore. It is explicit by default because not
every process tree is checkpointable. Runtime sessions use `pivot_root` rather
than plain `chroot`, workload stdio is redirected to an internal workspace log,
and hostd resolves child PIDs to the PID namespace root before calling CRIU.
That keeps checkpoint images free of host log descriptors and matches CRIU's
container restore model.

`process-checkpoint-create` uses CRIU to dump a selected live workload process
tree into:

```text
/var/lib/gema-system/sessions/<session>/process-checkpoints/<checkpoint_id>
/var/lib/gema-system/sessions/<session>/process_checkpoints.json
```

If no PID is supplied, hostd selects a live workload process from the session
cgroup. If a child PID is supplied, hostd records it as `requested_pid` and
checkpoints the namespace root that owns the process tree. Bind mounts are
dumped as external mounts, and the session network namespace is inherited during
restore.

`process-checkpoint-restore` restores a completed CRIU image with the session
rootfs and records the restored PID when CRIU reports one. Restore is fail-closed
when the original checkpoint source is still running. Use `terminate-existing`
for recovery semantics, where hostd stops the original process tree before
restore. Use `allow-live` only for deliberate clone semantics.

The default smoke covers a long-running Python process and verifies that the
restored process continues updating workspace state after restore. The extended
matrix is available for broader CRIU coverage:

```text
GEMA_SMOKE_CRIU_EXTENDED=1 sudo /root/gema-system/scripts/smoke-runtime.sh
```

That extended gate exercises a listening Python HTTP server, a multi-process
Python tree, and a package-mutated rootfs before checkpoint/restore.

Local snapshots can include an attempted process checkpoint by setting:

```text
GEMA_PROCESS_CHECKPOINT_ON_SNAPSHOT=1
```

The checkpoint result is stored inside the snapshot details under
`process_checkpoint`. Failures are recorded clearly and do not corrupt the
workspace, runtime-diff, or session-state layers.

Each completed snapshot records size and SHA-256 checksums. Restore verifies
the workspace archive checksum before extracting into the mounted workspace and
verifies the runtime-diff checksum before restoring the overlay upperdir. Runtime
diff restore requires the session to be stopped so the overlay upperdir is not
modified while mounted.

Launches use a per-session overlay rootfs by default:

```text
/var/lib/gema-system/runtime-rootfs/<session>/merged
/var/lib/gema-system/runtime-rootfs/<session>/upper
/var/lib/gema-system/runtime-rootfs/<session>/work
/var/lib/gema-system/sessions/<session>/runtime_rootfs.json
```

The base rootfs remains clean. User-level runtime changes outside the workspace
are isolated to the session upperdir and become part of the snapshot runtime
diff. Root-owned package installs remain governed by the no-root session model;
Gema exposes a controlled package installer for that path instead of giving the
workspace user root.

## Gema Package Installer

Package installs are hostd-owned actions:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh package-install web-1 apt jq ffmpeg
sudo /root/gema-system/scripts/gema-hostd.sh package-install web-1 python transformers accelerate
sudo /root/gema-system/scripts/gema-hostd.sh package-install web-1 pip rich
sudo /root/gema-system/scripts/gema-hostd.sh package-install web-1 npm typescript
sudo /root/gema-system/scripts/gema-hostd.sh package-install web-1 node vite
sudo /root/gema-system/scripts/gema-hostd.sh package-list web-1
sudo /root/gema-system/scripts/gema-hostd.sh package-verify-lock web-1 latest
sudo /root/gema-system/scripts/gema-hostd.sh package-replay-lock web-1 latest
```

The installer is intentionally structured. It does not expose arbitrary shell
execution. Hostd prepares or remounts the session overlay rootfs, then
`scripts/gema-package-install.sh` runs the requested manager inside that rootfs.
Install records are stored at:

```text
/var/lib/gema-system/sessions/<session>/package-installs/packages.jsonl
/var/lib/gema-system/sessions/<session>/package-installs/latest-lock.json
/var/lib/gema-system/sessions/<session>/package-installs/locks/
```

Every successful install writes a `gema.package_lock.v1` record with the exact
resolved apt package set, the managed Python `pip freeze`, the global npm tree,
and checksums for the exported lock artifacts. `package-list` returns the
history, available locks, and the latest lock so the control plane can show or
export what changed.

`package-verify-lock` is read-only. It compares the current session rootfs
against a selected lock and fails on drift: missing packages, extra packages, or
changed versions.

`package-replay-lock` is the convergence operation. It reinstalls from a
selected lock, prunes package-manager extras, then runs the same drift check
before returning success. Its convergence scope is intentionally limited to
package-manager-owned state: Python replay removes packages outside the lock
from the selected Gema ML lane under `/opt/gema/ml/<lane>`; npm replay removes
global packages outside the lock; apt replay purges non-essential apt packages
not present in the lock and then runs autoremove. It does not revert arbitrary
workspace files, user data, model artifacts, or runtime files outside
package-manager ownership. Both commands accept `latest`, a lock filename inside
`package-installs/locks`, or a package lock path under the session
package-install directory.

Policy:

```text
apt
  allowlist by default
  blocks kernel, init, daemon, firewall, container runtime, sudo, and SSH server packages
  uses --no-install-recommends
  blocks service autostart inside the rootfs with policy-rc.d

python / pip
  bootstraps python3-venv through the apt policy if needed
  uses the selected Gema-managed Python lane under /opt/gema/ml/<lane>
  validates package names
  rejects URL/path/direct-reference installs by default
  installs binary wheels through pip inside that managed environment
  exposes python, python3, pip, and pip3 naturally through /opt/gema/bin

npm / node
  bootstraps nodejs and npm through the apt policy if needed
  validates package names
  installs npm packages globally inside the session rootfs
  runs with npm lifecycle scripts disabled by default
```

The stricter defaults can be widened by operator policy only:

```text
GEMA_PIP_ONLY_BINARY=""        allow source builds
GEMA_NPM_IGNORE_SCRIPTS=0      allow npm lifecycle scripts
GEMA_APT_ALLOW_ALL=1           bypass the apt allowlist while keeping deny rules
```

The session must be stopped before package installation if it is currently
running. Package changes land in:

```text
/var/lib/gema-system/runtime-rootfs/<session>/upper
```

and are captured automatically by the next `<snapshot>.runtime-diff.tar`.

Useful policy environment variables:

```text
GEMA_APT_ALLOWLIST
GEMA_APT_ALLOW_ALL=1
GEMA_APT_DENY_REGEX
GEMA_PYTHON_DENY_REGEX
GEMA_NPM_DENY_REGEX
GEMA_PACKAGE_TIMEOUT_SECONDS
GEMA_MANAGE_APT_SOURCES
```

R2 integration:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh r2-configure web-1 r2:gema-cloud-volumes users/<user>/sessions/<session>
sudo /root/gema-system/scripts/gema-hostd.sh r2-status web-1 probe
sudo /root/gema-system/scripts/gema-hostd.sh r2-sync web-1 workspace push
sudo /root/gema-system/scripts/gema-hostd.sh r2-sync web-1 snapshots push
sudo /root/gema-system/scripts/gema-hostd.sh r2-mount web-1 workspace
sudo /root/gema-system/scripts/gema-hostd.sh r2-unmount web-1 workspace
```

R2 state is stored at:

```text
/var/lib/gema-system/sessions/<session>/r2_state.json
```

Sync and mount events are recorded in the runtime ledger and include storage
accounting. `r2-mount` creates a real FUSE mount backed by `rclone mount` under:

```text
/var/lib/gema-system/r2mounts/<session>/<workspace|snapshots>
```

R2 failure injection is covered by the smoke gate when credentials are present:

```bash
GEMA_SMOKE_R2=1 GEMA_SMOKE_R2_FAILURES=1 sudo /root/gema-system/scripts/smoke-runtime.sh
```

That lane pushes a valid snapshot, deliberately corrupts and truncates the
remote snapshot object, verifies missing-object restore failure, confirms
restore fails on checksum verification for bad objects, then restores the good
object. It also pushes a second snapshot and confirms remote `snapshots.json`
retains both snapshot records before normal workspace push/pull validation.

Agent API:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh agent-api web-1 facts
sudo /root/gema-system/scripts/gema-hostd.sh agent-api web-1 snapshot-create '{"snapshot_id":"agent-1"}'
sudo /root/gema-system/scripts/gema-hostd.sh agent-socket web-1
```

Agent API config and audit:

```text
/var/lib/gema-system/sessions/<session>/agent_api.json
/var/lib/gema-system/sessions/<session>/agent.audit.jsonl
```

The workspace transport is a per-session Unix socket created at:

```text
/var/lib/gema-system/workspaces/<session>/agent/hostd.sock
```

Inside the substrate it appears as:

```text
/home/gema/.gema/agent/hostd.sock
```

The socket accepts controlled JSON actions and returns runtime facts, lifecycle,
metrics, storage, process supervisor, snapshots, recovery plan, R2 state, and
GPU assignment.

GPU assignment:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh assign-gpu web-1 nvidia 0 exclusive
sudo /root/gema-system/scripts/gema-hostd.sh gpu-status web-1
```

GPU assignment is stored at:

```text
/var/lib/gema-system/sessions/<session>/gpu_assignment.json
```

Assignment requires the relevant GPU host probe gate to pass. Launch injects the
assignment into the session environment, including `CUDA_VISIBLE_DEVICES` for
NVIDIA. It also bind-mounts the assigned `/dev/nvidia*`, `/dev/dri`, or
`/dev/kfd` device paths into the substrate. Metrics include assignment and live
GPU readings where available.

## Port Allocation Service

Hostd owns public port allocation. Launch no longer needs a hardcoded port:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh launch web-1 auto 8000 -- \
  /bin/bash -lc 'python3 -m http.server 8000 --bind "$GEMA_GUEST_IP"'
```

The allocator tracks:

```text
/var/lib/gema-system/ports/allocations.json
/var/lib/gema-system/ports/audit.jsonl
/var/lib/gema-system/sessions/<session>/port_allocations.json
```

Commands:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh ports
sudo /root/gema-system/scripts/gema-hostd.sh allocate-port web-1 8000 public auto tcp
sudo /root/gema-system/scripts/gema-hostd.sh allocate-port web-1 9000 private
sudo /root/gema-system/scripts/gema-hostd.sh release-ports web-1
```

Public ports are selected from `GEMA_PORT_POOL_START..GEMA_PORT_POOL_END`
(`20000..29999` by default). Requested public ports are allowed only when they
are not reserved, not already allocated, and not already bound on the host.

Private ports are recorded for audit and service discovery but are not exposed
on the host and do not create a router.

Reserved platform ports include SSH, HTTP/S, Postgres, Redis, Gema API/web,
TURN, VNC, Sunshine/Moonlight control ports, Ollama, and other host services.

Stopping or cleaning up a session releases all of its public and private port
allocations. Allocation, conflict, and release events are written to both the
global port audit log and the session runtime event ledger.

## Process Supervisor

Hostd records the main substrate process, router/service processes, and
workload processes discovered from the session cgroup:

```text
/var/lib/gema-system/sessions/<session>/processes.json
```

Inspect or refresh the supervisor view:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh processes web-1
sudo /root/gema-system/scripts/gema-hostd.sh inspect web-1
```

Each record includes:

```text
kind
role
command
pid
status
exit_code
started_at
finished_at
restart_policy
crash_reason
log_path
metadata
```

Active records move into `history` when the process exits or when the session is
stopped. Exits are also written to the runtime ledger as
`process_supervisor_exit`.

## Rootfs/Image Manager

Hostd exposes a catalog of usable OS profiles:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh images
sudo /root/gema-system/scripts/gema-hostd.sh image-compat noble-ml-cu121 cpu gema-native
sudo /root/gema-system/scripts/gema-hostd.sh image-compat any nvidia gema-native 12.3
```

The generated catalog is persisted at:

```text
/var/lib/gema-system/images/catalog.json
```

Each profile includes:

```text
id
profile
rootfs
version
checksum
os release details
installed capabilities
package count and key package versions
runtime compatibility
GPU vendor/CUDA compatibility requirements
```

Rootfs builds also write a local manifest:

```text
/var/lib/gema-system/rootfs/<profile>/etc/gema-image.json
```

GPU ML profiles are wheel-only by default:

```text
noble-ml-cu118
noble-ml-cu121
noble-ml-cu124
```

These profiles do not carry `/usr/local/cuda-*` toolkits. The NVIDIA driver and
`libcuda.so.1` come from the GPU host at launch time, while PyTorch/vLLM-style
CUDA runtime dependencies come from the selected Python wheel lane. Full CUDA
toolkit images should be kept as separate development profiles, not used as the
default GPU session image.

ML profiles are launched through the Gema ML bridge automatically. Users still
submit their original command; hostd records that command as-is and runs the
actual process through:

```text
/opt/gema/bin/gema-ml-bridge run -- <user command>
```

The bridge is intentionally small. It writes runtime state, heartbeat,
pause/ack records, and checkpoint manifests into the workspace and mirrors the
control-plane-visible files under:

```text
/var/lib/gema-system/workspaces/<session>/agent/ml-bridge
```

This gives Gema a cooperative safe-point model for GPU work: it can request a
pause, collect framework checkpoint metadata, and detach the GPU attachment
without pretending to freeze arbitrary CUDA VRAM or remote peer state.

Training-like commands are bridge-enforced. The bridge refuses to run if the
ML recovery contract or framework checkpoint policy is missing. This makes the
hostd-created `.gema/recovery/resume.json` and
`.gema/recovery/framework-checkpoint-policy.json` part of the execution path,
not only advisory metadata.

GPU-bound commands are also bridge-enforced. The CPU workspace may start and
remain usable while provider GPU provisioning, GVPhase1 backend startup, SSH
tunnel setup, and real GPU validation are still in progress. During that window,
normal CPU preparation work can run, but commands that require CUDA/GPU service
execution fail closed until `.gema/fabric/cuda_proxy_state.json` reports:

```json
{"ready": true, "status": "validated"}
```

The bridge writes `blocked_gpu_not_ready` state and emits a fabric event instead
of letting `torchrun`, DeepSpeed, vLLM, CUDA probes, or similar GPU-heavy
commands start against a not-yet-attached accelerator.

Real GPU validation must be run on NVIDIA workers before marking a lane
production-ready:

```bash
sudo /root/gema-system/scripts/gema-ml-gpu-validate.sh
sudo GEMA_GPU_VALIDATE_LANES="cu118 cu121 cu124" /root/gema-system/scripts/gema-ml-gpu-validate.sh
```

The validation runner checks `torch.cuda.is_available()`, a CUDA tensor op,
bitsandbytes import, xformers import, DeepSpeed import, NCCL single-process
init/all-reduce, and vLLM import on `cu121`/`cu124`.

## GVPhase1 Remote CUDA Proxy

Gema ML rootfs images now carry GVPhase1 as the first-class remote CUDA proxy
data plane:

```bash
/opt/gema/gvphase1/lib/libcuda.so.1
/opt/gema/gvphase1/lib/libcudart.so
/opt/gema/gvphase1/lib/libcublas.so
/opt/gema/gvphase1/lib/libcudnn.so
/opt/gema/gvphase1/lib/libcurand.so
/opt/gema/gvphase1/lib/libcufft.so
/opt/gema/gvphase1/lib/libnvrtc.so
/opt/gema/gvphase1/lib/libnvidia-ml.so
/opt/gema/gvphase1/bin/gvphase1-backend
/opt/gema/bin/gvphase1-backend
/opt/gema/bin/gema-gvphase1-env
```

The old prototype remains only as a legacy compatibility artifact:

```bash
/opt/gema/cuda-proxy/lib/libcuda.so.1
/opt/gema/cuda-proxy/bin/gema-cuda-proxy-daemon
/opt/gema/bin/gema-cuda-proxy-daemon
/opt/gema/bin/gema-cuda-proxy-env
```

GVPhase1 implements a binary RPC protocol and frontend libraries for CUDA
Driver, CUDA Runtime, cuBLAS/cuBLASLt, cuDNN, cuRAND, cuFFT, cuSPARSE,
cuSOLVER, NVRTC, and NVML. The backend daemon runs beside the real NVIDIA
driver on the attached GPU node. The workspace sets `LD_LIBRARY_PATH` to
`/opt/gema/gvphase1/lib` and routes calls over `GVPHASE1_ENDPOINT`.

Current production gate:

```bash
/root/gema-system/scripts/smoke-cuda-proxy.sh
/root/gema_cloud/scripts/smoke-vast-cuda-proxy.py
```

The broad real-GPU probe covers CUDA driver/runtime, module load, kernel launch,
streams/events, malloc/memcpy/memset/async copy, cuBLAS `cublasSgemm_v2`,
cuRAND, cuFFT, NVRTC, NVML, and cuDNN descriptor/convolution/softmax paths.
Unsupported GPU features, such as MIG on RTX 4090, are recorded as skipped only
when the backend reports a device-level not-supported result.

The current checksum is a deterministic manifest checksum over `os-release`,
`gema-release`, `.gema-rootfs-ready`, and `dpkg/status`. It is intended for
version identity and drift detection. Full filesystem Merkle manifests can be
added later for content-addressed distribution.

The daemon is implemented in Rust behind the shell wrapper while keeping the
same Unix-socket control contract.

## Verified Smoke

The daemon has been verified with:

```bash
sudo /root/gema-system/scripts/gema-hostd.sh start
sudo /root/gema-system/scripts/gema-hostd.sh launch hostd-smoke 18081 8000 -- \
  /bin/bash -lc 'echo hostd-ok > index.html; python3 -m http.server 8000 --bind "$GEMA_GUEST_IP"'
curl http://127.0.0.1:18081/
```

Expected response:

```text
hostd-ok
```
