# Gema Runtime

Gema Runtime is the host-side execution layer for Gema OS.

It is not a Docker shim. It is the control surface that lets Gema own the Linux
lifecycle directly:

- GPU discovery and compatibility gates
- cgroup resource enforcement
- workspace filesystem snapshots
- service port allocation
- task and process supervision
- recovery contracts
- storage mounts
- agent-visible runtime state

The runtime starts with OCI compatibility through `runc`/`buildah`, then moves
the Gema-specific pieces out of container entrypoint scripts and into a daemon
that speaks Gema directly.

## Current Host

The current server has:

- `runc`
- `buildah`
- `btrfs`

Missing on this host:

- NVIDIA driver/runtime tools
- NVIDIA Container Toolkit
- ROCm tools
- `crun`
- ZFS

That is acceptable for control-plane development. GPU hosts must pass the host
probe before running paid GPU workspaces.

## First Runtime Contract

Every workspace gets a `GemaWorkloadSpec`:

- image/profile
- user id and session id
- CPU, RAM, disk, storage quota
- GPU vendor and required capabilities
- reserved transport ports
- user service ports
- workspace mount
- agent mount
- checkpoint policy
- recovery policy

Docker can still exist in the old Gema Cloud product path while this matures,
but Gema System itself is a native Linux substrate.

The first native lane is documented in `docs/SUBSTRATE.md`.
