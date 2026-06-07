# Gema Runtime Architecture

## Objective

Gema Runtime is the machine-level supervisor for Gema OS. It turns a raw CPU or
GPU host into a Gema machine with predictable lifecycle semantics.

The runtime owns:

- machine capability inventory
- NVIDIA and AMD runtime gates
- image/profile selection
- workload launch
- port routing
- filesystem snapshots
- recovery contracts
- process supervision
- task ledgers
- agent runtime APIs

## Runtime Layers

### 1. Host Probe

Detects:

- kernel version
- cgroup mode
- namespace support
- seccomp/AppArmor availability
- `runc`, `crun`, `buildah`
- btrfs/zfs/overlayfs support
- NVIDIA driver, CUDA compatibility, NVENC capability
- AMD ROCm capability

No GPU workspace launches until the host probe produces a compatible verdict.

### 2. Workload Contract

A workload is not only an image. It is a contract:

- resources
- GPU requirements
- storage paths
- ports
- checkpoint policy
- recovery mode
- agent permissions

This prevents random containers from becoming ungoverned compute.

### 3. Port Plane

Gema reserves internal platform ports:

- desktop transport
- SSH gateway
- agent bridge
- model runtime
- API sidecars

User service ports live in a separate safe range and are routed by Gema. Users
do not choose public IP mode from the UI. They choose a service name and an
internal port. The runtime allocates the outside route.

### 4. GPU Plane

NVIDIA path:

- host driver verification
- CUDA lane compatibility
- NVENC check
- device assignment
- MIG support later

AMD path:

- ROCm verification
- device assignment
- HIP runtime check
- encoder capability check where available

### 5. Snapshot Plane

Filesystem:

- btrfs/zfs snapshots where available
- overlay layer delta fallback
- R2 object manifest persistence

Application:

- ML checkpoint contracts
- Jupyter state
- ComfyUI workflows
- Blender autosaves
- service restart contracts

Process:

- CRIU where the workload is compatible
- never promised as the only recovery layer for GPU state

### 6. Agent Plane

The agent does not guess. It asks the runtime:

- what is running
- what crashed
- which checkpoint is valid
- which command started the job
- which service ports exist
- which recovery plan is allowed

The runtime gives structured facts, not terminal folklore.

### 7. Execution Fabric And CUDA Data Plane

Gema treats the workspace as the persistent runtime identity and the GPU as a
detachable accelerator. The architecture now separates control from compute:
the fabric bridge is control-plane infrastructure, while GVPhase1 is the CUDA
API remoting data plane that makes an attached remote GPU visible to local
workspace processes.

Each session can now record:

- `execution_contract.json`: command intent, image lane, GPU requirements,
  checkpoint policy, recovery plan, and state ownership
- `accelerator_state.json`: attach/detach status, compatible CUDA lane, GPU
  assignment, and phase-by-phase readiness
- `cuda_proxy_state.json`: GVPhase1 endpoint, validation status, frontend
  library contract, backend placement, and real-GPU probe result
- `runtime.events.jsonl`: contract writes, attach requests, blocked attach
  decisions, and detach events

The contract and accelerator state are mirrored into `.gema/fabric/` inside the
workspace and into the agent mirror, so an accelerator worker can mount the
workspace and consume the same durable contract without querying hidden host
state.

State ownership is explicit:

- workspace files belong to the workspace volume
- image truth belongs to the image manifest
- checkpoint truth belongs to checkpoint policy and snapshot manifests
- command intent belongs to `hostd.json` and the runtime event ledger
- GPU state belongs only to the active execution worker
- agent decisions belong to the runtime event ledger

This is the production boundary:

- `gema-fabric-bridge` is control plane: provisioning, SSH tunnel setup, file
  transfer, heartbeats, logs, checkpoints, and recovery coordination.
- `gema-accelerator-worker` is coarse task offload fallback.
- GVPhase1 is the primary CUDA data plane for terminal-visible remote GPU.

A true twin attach requires GVPhase1 readiness, not merely an SSH worker. The
GPU node runs `gvphase1-backend` beside the real NVIDIA driver. The Gema
workspace uses `/opt/gema/gvphase1/lib` so local CUDA/PyTorch probes resolve to
the proxy libraries and reach the attached GPU over the managed endpoint.

`gema-accelerator-worker` remains installed for workloads that do not need
transparent CUDA. It validates `.gema/fabric/execution_contract.json`, checks
recovery policy, exports the selected CUDA lane, runs through the ML bridge when
available, and writes final worker state back into `.gema/fabric/`.
