# Gema Linux Substrate

This is the native path.

It does not execute workloads through Docker, Podman, Buildah, or runc.

It uses Linux directly:

- debootstrap rootfs
- cgroup v2
- mount namespace
- pid namespace
- ipc namespace
- uts namespace
- `pivot_root`
- bind-mounted workspace
- bind-mounted agent state
- private network namespace
- host veth to guest veth
- selected TCP port routing
- host-owned process ledger
- Rust hostd preflight before shell substrate mutation

## Rust Preflight Boundary

`hostd` now writes a typed substrate preflight before it allocates public ports
or starts `gema-substrate-run.sh`:

```text
/var/lib/gema-system/sessions/<session>/substrate-preflight.json
```

The preflight validates the managed rootfs, workspace mountpoint, agent state
directory, launch argv, required host tools, substrate executable, and hardening
requirements. Failure is recorded as a `substrate_preflight_failed` runtime
event and hostd rolls back workspace/runtime-rootfs mounts before returning the
launch error.

This keeps the current shell substrate working while moving the critical launch
contract into Rust, where failures are structured and auditable.

## Network Cleanup

Each networked session records its namespace, veth pair, guest address, subnet,
and NAT rule in:

```text
/var/lib/gema-system/sessions/<session>/network.json
```

Cleanup removes the session MASQUERADE rule from `iptables -t nat
POSTROUTING`, deletes the host veth, and deletes the named netns. If
`network.json` is missing, cleanup derives the deterministic subnet and veth
name from the session id so failed partial launches can still be cleaned.
Substrate startup also rejects a live subnet collision before touching network
state.

The smoke gate checks NAT/netns cleanup after normal cleanup, restart recovery,
UDP routing, and failed launch paths.

## Rootfs Isolation

Launches use a per-session overlay rootfs:

```text
/var/lib/gema-system/runtime-rootfs/<session>/merged
```

This prevents session identity files such as `/etc/hostname` and `/etc/hosts`
from being written into the shared managed base rootfs. Direct shared-rootfs
launch is disabled in hostd; operator environment overrides such as
`GEMA_ALLOW_SHARED_ROOTFS` and `GEMA_RUNTIME_OVERLAY=0` are ignored by the
production launch path.

The smoke gate launches two sessions concurrently from the same base profile and
asserts that they receive different merged rootfs paths and different identity
files while the base rootfs remains unchanged.

## Build Rootfs

```bash
sudo /root/gema-system/scripts/gema-rootfs-build.sh
```

The Noble ML images are built as wheel-only CUDA ABI profiles. The runtime-owned
requirements live at:

```text
/root/gema-system/rootfs/ml-requirements.txt
```

The default builder installs system prerequisites and the Gema ML stack without
full CUDA toolkits. GPU sessions rely on the host NVIDIA driver for
`libcuda.so.1`; Python packages use CUDA-enabled wheels inside the rootfs.

Python ML environments are lane-specific under `/opt/gema/ml/<lane>`. Production
GPU images are split into one profile per ABI lane:

```text
/var/lib/gema-system/rootfs/noble-ml-cu118
/var/lib/gema-system/rootfs/noble-ml-cu121
/var/lib/gema-system/rootfs/noble-ml-cu124
```

Each profile carries a single lane with exact
`torch==2.5.1+cu*`, `torchvision==0.20.1+cu*`, and
`torchaudio==2.5.1+cu*` wheels, then applies the full
`/opt/gema/ml-requirements.txt` stack and records a freeze file per lane.
`/opt/gema/bin/gema-cuda-select` chooses the best installed Python lane from
`GEMA_CUDA_LANE`, `GEMA_HOST_CUDA_VERSION`, or `nvidia-smi`. The `python`,
`python3`, `pip`, and `pip3` wrappers in `/opt/gema/bin` route through that
selector.

The copied requirements, CUDA lane manifest, selector, and build inputs are
recorded inside each ML rootfs as `/opt/gema/ml-requirements.txt`,
`/opt/gema/cuda-lanes.json`, and `.gema-ml-ready`; changing any of those inputs
forces the ML layer to be reapplied on the next rootfs build.

The legacy full CUDA toolkit rootfs is not the default GPU profile. If a CUDA
development image is needed for `nvcc`, custom CUDA extensions, or CUDA C++
workloads, build a separate `cuda-dev` profile with
`GEMA_INSTALL_CUDA_TOOLKITS=1`.

## Run Smoke

```bash
sudo GEMA_SESSION_ID=smoke-substrate \
  /root/gema-system/scripts/gema-substrate-run.sh run -- \
  /bin/bash -lc 'id && pwd && echo ok > /home/gema/workspace/native.txt'
```

## Inspect

```bash
sudo /root/gema-system/scripts/gema-substrate-run.sh inspect smoke-substrate
```

## Network And Ports

Every networked session receives its own Linux network namespace and a private
guest address. The runtime injects these variables into the workload:

```text
GEMA_NETWORK=1
GEMA_HOST_IP=<host-side veth address>
GEMA_GUEST_IP=<guest-side veth address>
```

Expose ports with `GEMA_PORT_MAPS`:

```bash
sudo GEMA_KEEP=1 \
  GEMA_SESSION_ID=net-smoke \
  GEMA_PORT_MAPS=18080:8000/tcp \
  /root/gema-system/scripts/gema-substrate-run.sh run -- \
  /bin/bash -lc 'python3 -m http.server 8000 --bind "$GEMA_GUEST_IP"'
```

Then verify from the host:

```bash
curl http://127.0.0.1:18080/
```

## Meaning

Gema owns the machine boundary. The workload is not a Docker container. It is a
Gema Linux substrate session.

Docker may remain in the old product path until this runtime replaces it, but it
is not part of this lane.
