#!/usr/bin/env bash
set -euo pipefail

ROOT="${GEMA_RUNTIME_ROOT:-/var/lib/gema-runtime}"
IMAGE="${GEMA_IMAGE:-docker-daemon:gema-base:latest}"
SESSION_ID="${GEMA_SESSION_ID:-native-$(date +%s)-$$}"
USER_ID="${GEMA_USER_ID:-local}"
WORKSPACE="${GEMA_WORKSPACE:-$ROOT/workspaces/$SESSION_ID/workspace}"
AGENT_DIR="${GEMA_AGENT_DIR:-$ROOT/workspaces/$SESSION_ID/agent}"
MEMORY_MAX="${GEMA_MEMORY_MAX:-1073741824}"
CPU_QUOTA="${GEMA_CPU_QUOTA:-100000}"
CPU_PERIOD="${GEMA_CPU_PERIOD:-100000}"
KEEP="${GEMA_KEEP:-0}"

usage() {
  cat >&2 <<'EOF'
usage:
  gema-native-run.sh run -- <command> [args...]
  gema-native-run.sh cleanup <session_id>

environment:
  GEMA_IMAGE        default docker-daemon:gema-base:latest
  GEMA_SESSION_ID   default native-<timestamp>
  GEMA_USER_ID      default local
  GEMA_WORKSPACE    default /var/lib/gema-runtime/workspaces/<session>/workspace
  GEMA_AGENT_DIR    default /var/lib/gema-runtime/workspaces/<session>/agent, mounted at /home/gema/.gema/agent
  GEMA_MEMORY_MAX   default 1073741824
  GEMA_CPU_QUOTA    default 100000
  GEMA_CPU_PERIOD   default 100000
  GEMA_KEEP         set 1 to preserve bundle/rootfs metadata after exit
EOF
}

require() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "ERROR: missing required command: $1" >&2
    exit 69
  }
}

json_escape() {
  python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$1"
}

resolve_image_source() {
  local image="$1"
  if [[ "$image" == docker-daemon:* ]]; then
    require docker
    local docker_image="${image#docker-daemon:}"
    local safe
    safe="$(printf '%s' "$docker_image" | tr '/:@' '___')"
    local archive_dir="$ROOT/images"
    local archive="$archive_dir/${safe}.tar"
    mkdir -p "$archive_dir"
    if [ ! -s "$archive" ]; then
      echo "[gema-native] exporting Docker image $docker_image to $archive" >&2
      docker save "$docker_image" -o "$archive"
    fi
    printf 'docker-archive:%s' "$archive"
  else
    printf '%s' "$image"
  fi
}

cleanup_session() {
  local id="$1"
  local base="$ROOT/sessions/$id"
  local cname="gema-native-$id"
  runc kill "$id" KILL >/dev/null 2>&1 || true
  runc delete -f "$id" >/dev/null 2>&1 || true
  if [ -f "$base/buildah.id" ]; then
    local bid
    bid="$(cat "$base/buildah.id" 2>/dev/null || true)"
    [ -n "$bid" ] && buildah umount "$bid" >/dev/null 2>&1 || true
    [ -n "$bid" ] && buildah rm "$bid" >/dev/null 2>&1 || true
  fi
  buildah rm "$cname" >/dev/null 2>&1 || true
  rm -rf "$base"
}

prepare_bundle() {
  require buildah
  require runc
  require python3

  local base="$ROOT/sessions/$SESSION_ID"
  local bundle="$base/bundle"
  local cname="gema-native-$SESSION_ID"
  mkdir -p "$bundle" "$WORKSPACE" "$AGENT_DIR" "$base/logs"

  cleanup_session "$SESSION_ID"
  mkdir -p "$bundle" "$WORKSPACE" "$AGENT_DIR" "$base/logs"

  local image_source
  image_source="$(resolve_image_source "$IMAGE")"
  echo "[gema-native] importing image=$image_source" >&2
  local bid
  bid="$(buildah from --name "$cname" "$image_source")"
  echo "$bid" > "$base/buildah.id"

  local rootfs
  rootfs="$(buildah mount "$bid")"
  echo "$rootfs" > "$base/rootfs.path"

  (
    cd "$bundle"
    runc spec
  )

  python3 - "$bundle/config.json" "$rootfs" "$WORKSPACE" "$AGENT_DIR" "$SESSION_ID" "$USER_ID" "$MEMORY_MAX" "$CPU_QUOTA" "$CPU_PERIOD" <<'PY'
import json
import sys
from pathlib import Path

config_path, rootfs, workspace, agent_dir, session_id, user_id, memory_max, cpu_quota, cpu_period = sys.argv[1:]
path = Path(config_path)
spec = json.loads(path.read_text())
spec["ociVersion"] = spec.get("ociVersion") or "1.0.2"
spec["root"] = {"path": rootfs, "readonly": False}
spec["hostname"] = f"gema-{session_id[:12]}"
spec["process"]["terminal"] = False
spec["process"]["cwd"] = "/home/gema/workspace"
spec["process"]["user"] = {"uid": 1000, "gid": 1000}
spec["process"]["env"] = [
    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "HOME=/home/gema",
    "USER=gema",
    "GEMA_NATIVE_RUNTIME=1",
    f"SESSION_ID={session_id}",
    f"USER_ID={user_id}",
    "GEMA_WORKSPACE_ROOT=/home/gema",
    "GEMA_PROJECT_DIR=/home/gema/workspace",
    "GEMA_AGENT_DIR=/home/gema/workspace/.gema/agent",
]
spec["linux"]["resources"] = {
    "memory": {"limit": int(memory_max)},
    "cpu": {"quota": int(cpu_quota), "period": int(cpu_period)},
}
spec["linux"]["namespaces"] = [
    {"type": "pid"},
    {"type": "ipc"},
    {"type": "uts"},
    {"type": "mount"},
    {"type": "cgroup"},
]
spec["linux"]["maskedPaths"] = [
    "/proc/acpi",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/proc/sched_debug",
    "/sys/firmware",
]
spec["linux"]["readonlyPaths"] = [
    "/proc/asound",
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
]
mounts = spec.get("mounts", [])
mounts.extend([
    {
        "destination": "/home/gema/workspace",
        "type": "bind",
        "source": workspace,
        "options": ["rbind", "rw"],
    },
    {
        "destination": "/home/gema/.gema/agent",
        "type": "bind",
        "source": agent_dir,
        "options": ["rbind", "rw"],
    },
    {
        "destination": "/tmp",
        "type": "tmpfs",
        "source": "tmpfs",
        "options": ["nosuid", "nodev", "mode=1777", "size=268435456"],
    },
])
seen = set()
deduped = []
for mount in mounts:
    dest = mount.get("destination")
    if dest in seen:
        continue
    seen.add(dest)
    deduped.append(mount)
spec["mounts"] = deduped
path.write_text(json.dumps(spec, indent=2))
PY

  printf '%s\n' "$bundle"
}

run_command() {
  shift
  [ "${1:-}" = "--" ] && shift
  if [ "$#" = "0" ]; then
    set -- /bin/bash -lc 'id && pwd && ls -la /home/gema/workspace'
  fi

  local bundle
  bundle="$(prepare_bundle)"

  python3 - "$bundle/config.json" "$@" <<'PY'
import json
import sys
from pathlib import Path

config = Path(sys.argv[1])
argv = sys.argv[2:]
spec = json.loads(config.read_text())
spec["process"]["args"] = argv
config.write_text(json.dumps(spec, indent=2))
PY

  echo "[gema-native] running session=$SESSION_ID workspace=$WORKSPACE"
  set +e
  runc run --bundle "$bundle" "$SESSION_ID"
  status=$?
  set -e

  if [ "$KEEP" != "1" ]; then
    cleanup_session "$SESSION_ID"
  else
    echo "[gema-native] kept session files at $ROOT/sessions/$SESSION_ID"
  fi
  exit "$status"
}

case "${1:-}" in
  run) run_command "$@" ;;
  cleanup)
    [ -n "${2:-}" ] || { usage; exit 64; }
    cleanup_session "$2"
    ;;
  *) usage; exit 64 ;;
esac
