#!/usr/bin/env bash
set -euo pipefail

ROOT="${GEMA_SYSTEM_ROOT:-/var/lib/gema-system}"
SESSION_ID="${GEMA_SESSION_ID:-substrate-$(date +%s)-$$}"
USER_ID="${GEMA_USER_ID:-local}"
session_uid_hex="$(printf '%s' "$SESSION_ID" | tr -cd 'A-Fa-f0-9' | cut -c1-8)"
if [ -n "$session_uid_hex" ]; then
  session_uid_seed=$((16#$session_uid_hex))
else
  session_uid_seed="$(printf '%s' "$SESSION_ID" | cksum | awk '{print $1}')"
fi
GUEST_UID="${GEMA_GUEST_UID:-$((200000 + (session_uid_seed % 400000)))}"
GUEST_GID="${GEMA_GUEST_GID:-$GUEST_UID}"
export GUEST_UID GUEST_GID
DEFAULT_ROOTFS="$ROOT/rootfs/noble-base"
if [ ! -f "$DEFAULT_ROOTFS/.gema-rootfs-ready" ] && [ -f "$ROOT/rootfs/noble-ml-cu121/.gema-rootfs-ready" ]; then
  DEFAULT_ROOTFS="$ROOT/rootfs/noble-ml-cu121"
fi
ROOTFS="${GEMA_ROOTFS:-$DEFAULT_ROOTFS}"
WORKSPACE="${GEMA_WORKSPACE:-$ROOT/workspaces/$SESSION_ID/workspace}"
AGENT_DIR="${GEMA_AGENT_DIR:-$ROOT/workspaces/$SESSION_ID/agent}"
MEMORY_MAX="${GEMA_MEMORY_MAX:-1073741824}"
CPU_MAX="${GEMA_CPU_MAX:-100000 100000}"
PIDS_MAX="${GEMA_PIDS_MAX:-512}"
KEEP="${GEMA_KEEP:-0}"
PRESERVE_SESSION="${GEMA_PRESERVE_SESSION:-0}"
NETWORK="${GEMA_NETWORK:-1}"
PORT_MAPS="${GEMA_PORT_MAPS:-}"
DEVICE_BINDS="${GEMA_DEVICE_BINDS:-}"
DEVICE_ALLOWLIST="${GEMA_DEVICE_ALLOWLIST:-[]}"
DEVICE_POLICY="${GEMA_DEVICE_POLICY:-1}"
if [ -n "${GEMA_ROUTER_BIN:-}" ]; then
  ROUTER_BIN="$GEMA_ROUTER_BIN"
elif [ -x /root/gema-system/target/release/gema-runtime ]; then
  ROUTER_BIN="/root/gema-system/target/release/gema-runtime"
else
  ROUTER_BIN="/root/gema-system/target/debug/gema-runtime"
fi
HARDENING="${GEMA_HARDENING:-1}"
NO_NEW_PRIVS="${GEMA_NO_NEW_PRIVS:-1}"
DROP_CAPS="${GEMA_DROP_CAPS:-1}"
SECCOMP="${GEMA_SECCOMP:-1}"
ISOLATION_LANE="${GEMA_ISOLATION_LANE:-standard}"
APPARMOR_PROFILE="${GEMA_APPARMOR_PROFILE:-}"
SESSION_DIR="$ROOT/sessions/$SESSION_ID"
CGROUP="/sys/fs/cgroup/gema-system/$SESSION_ID"
NETNS="gema-$SESSION_ID"

usage() {
  cat >&2 <<'EOF'
usage:
  gema-substrate-run.sh run -- <command> [args...]
  gema-substrate-run.sh inspect <session_id>
  gema-substrate-run.sh cleanup <session_id>

This is Gema's native Linux substrate lane. It does not use Docker, Podman,
Buildah, or runc to execute the workload.
EOF
}

require_root() {
  if [ "$(id -u)" != "0" ]; then
    echo "ERROR: run as root" >&2
    exit 77
  fi
}

validate_session_id() {
  case "$1" in
    ""|.*|-*|_*|*/*|*..*)
      echo "ERROR: invalid session id" >&2
      exit 64
      ;;
  esac
  if [ "${#1}" -gt 80 ] || ! printf '%s' "$1" | grep -Eq '^[A-Za-z0-9][A-Za-z0-9_.-]*$'; then
    echo "ERROR: invalid session id" >&2
    exit 64
  fi
}

ensure_rootfs() {
  if [ ! -f "$ROOTFS/.gema-rootfs-ready" ]; then
    GEMA_ROOTFS="$ROOTFS" "$(dirname "$0")/gema-rootfs-build.sh"
  fi
}

mount_if_needed() {
  local src="$1"
  local dst="$2"
  local type="${3:-bind}"
  mkdir -p "$dst"
  if findmnt -rn -M "$dst" >/dev/null 2>&1; then
    return 0
  fi
  case "$type" in
    bind) mount --bind "$src" "$dst" ;;
    rbind) mount --rbind "$src" "$dst" ;;
    tmpfs) mount -t tmpfs -o nosuid,nodev,mode=1777,size="${GEMA_RUNTIME_TMPFS_SIZE:-4G}" tmpfs "$dst" ;;
    *) echo "unknown mount type: $type" >&2; exit 64 ;;
  esac
}

mount_file_if_needed() {
  local src="$1"
  local dst="$2"
  mkdir -p "$(dirname "$dst")"
  if findmnt -rn -M "$dst" >/dev/null 2>&1; then
    return 0
  fi
  [ -e "$dst" ] || : > "$dst"
  mount --bind "$src" "$dst"
}

install_file_if_changed() {
  local src="$1"
  local dst="$2"
  mkdir -p "$(dirname "$dst")"
  if [ ! -e "$dst" ] || ! cmp -s "$src" "$dst"; then
    install -m 0755 "$src" "$dst"
  fi
}

session_hostname() {
  printf 'gema-%s' "$SESSION_ID" | tr -c 'A-Za-z0-9-' '-' | cut -c1-63
}

write_runtime_identity() {
  local name="$1"
  mkdir -p "$ROOTFS/etc"
  printf '%s\n' "$name" > "$ROOTFS/etc/hostname"
  cat > "$ROOTFS/etc/hosts" <<EOF
127.0.0.1 localhost
127.0.1.1 $name
::1 localhost ip6-localhost ip6-loopback
EOF
}

prepare_guest_identity() {
  mkdir -p "$ROOTFS/etc"
  if [ -f "$ROOTFS/etc/passwd" ]; then
    if grep -q '^gema:' "$ROOTFS/etc/passwd"; then
      awk -F: -v OFS=: -v uid="$GUEST_UID" -v gid="$GUEST_GID" '$1=="gema" {$3=uid; $4=gid} {print}' "$ROOTFS/etc/passwd" > "$ROOTFS/etc/passwd.gema-tmp"
      mv "$ROOTFS/etc/passwd.gema-tmp" "$ROOTFS/etc/passwd"
    else
      printf 'gema:x:%s:%s:Gema Workspace:/home/gema:/bin/bash\n' "$GUEST_UID" "$GUEST_GID" >> "$ROOTFS/etc/passwd"
    fi
    chmod 0644 "$ROOTFS/etc/passwd" 2>/dev/null || true
  fi
  if [ -f "$ROOTFS/etc/group" ]; then
    if grep -q '^gema:' "$ROOTFS/etc/group"; then
      awk -F: -v OFS=: -v gid="$GUEST_GID" '$1=="gema" {$3=gid} {print}' "$ROOTFS/etc/group" > "$ROOTFS/etc/group.gema-tmp"
      mv "$ROOTFS/etc/group.gema-tmp" "$ROOTFS/etc/group"
    else
      printf 'gema:x:%s:\n' "$GUEST_GID" >> "$ROOTFS/etc/group"
    fi
    chmod 0644 "$ROOTFS/etc/group" 2>/dev/null || true
  fi
  if [ -f "$ROOTFS/etc/subuid" ]; then
    grep -v '^gema:' "$ROOTFS/etc/subuid" > "$ROOTFS/etc/subuid.gema-tmp" || true
    printf 'gema:%s:1\n' "$GUEST_UID" >> "$ROOTFS/etc/subuid.gema-tmp"
    mv "$ROOTFS/etc/subuid.gema-tmp" "$ROOTFS/etc/subuid"
  fi
  if [ -f "$ROOTFS/etc/subgid" ]; then
    grep -v '^gema:' "$ROOTFS/etc/subgid" > "$ROOTFS/etc/subgid.gema-tmp" || true
    printf 'gema:%s:1\n' "$GUEST_GID" >> "$ROOTFS/etc/subgid.gema-tmp"
    mv "$ROOTFS/etc/subgid.gema-tmp" "$ROOTFS/etc/subgid"
  fi
}

cleanup_mounts() {
  local id="$1"
  local rootfs="${GEMA_ROOTFS:-}"
  if [ -z "$rootfs" ] && [ -f "$ROOT/sessions/$id/meta.json" ]; then
    rootfs="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("rootfs",""))' "$ROOT/sessions/$id/meta.json" 2>/dev/null || true)"
  fi
  [ -n "$rootfs" ] || rootfs="$DEFAULT_ROOTFS"
  local bpf_pin=""
  bpf_pin="$(cat "$ROOT/sessions/$id/bpf/device.pin" 2>/dev/null || true)"
  if [ -n "$bpf_pin" ] && [ -d "/sys/fs/cgroup/gema-system/$id" ]; then
    bpftool cgroup detach "/sys/fs/cgroup/gema-system/$id" cgroup_device pinned "$bpf_pin" >/dev/null 2>&1 || true
  fi
  [ -n "$bpf_pin" ] && rm -f "$bpf_pin" >/dev/null 2>&1 || true
  rm -f "$ROOT/sessions/$id/bpf/device.pin" "$ROOT/sessions/$id/bpf/device.o" "$ROOT/sessions/$id/bpf/device.c" >/dev/null 2>&1 || true
  if [ -f "$ROOT/sessions/$id/device_binds" ]; then
    tac "$ROOT/sessions/$id/device_binds" 2>/dev/null | while IFS= read -r dst; do
      [ -n "$dst" ] || continue
      if findmnt -rn -M "$dst" >/dev/null 2>&1; then
        umount -l "$dst" || true
      fi
    done
    rm -f "$ROOT/sessions/$id/device_binds"
  fi
  if [ -f "$ROOT/sessions/$id/extra_binds" ]; then
    tac "$ROOT/sessions/$id/extra_binds" 2>/dev/null | while IFS= read -r dst; do
      [ -n "$dst" ] || continue
      if findmnt -rn -M "$dst" >/dev/null 2>&1; then
        umount -l "$dst" || true
      fi
    done
    rm -f "$ROOT/sessions/$id/extra_binds"
  fi
  for dst in \
    "$rootfs/usr/local/bin/gema-runtime" \
    "$rootfs/home/gema/.gema/agent" \
    "$rootfs/home/gema/workspace" \
    "$rootfs/tmp"; do
    if findmnt -rn -M "$dst" >/dev/null 2>&1; then
      umount -l "$dst" || true
    fi
  done
  rmdir "/sys/fs/cgroup/gema-system/$id" >/dev/null 2>&1 || true
}

mount_device_binds() {
  [ -n "$DEVICE_BINDS" ] || return 0
  : > "$SESSION_DIR/device_binds"
  IFS=',' read -r -a binds <<< "$DEVICE_BINDS"
  for binding in "${binds[@]}"; do
    [ -n "$binding" ] || continue
    local host_path guest_path dst
    host_path="${binding%%:*}"
    guest_path="${binding#*:}"
    [ "$guest_path" != "$binding" ] || guest_path="$host_path"
    case "$host_path:$guest_path" in
      /dev/*:/dev/*) ;;
      *) echo "ERROR: device binds are restricted to /dev paths: $binding" >&2; exit 66 ;;
    esac
    if [ ! -e "$host_path" ]; then
      echo "ERROR: device bind source missing: $host_path" >&2
      exit 66
    fi
    dst="$ROOTFS$guest_path"
    mkdir -p "$(dirname "$dst")"
    if [ -d "$host_path" ]; then
      mkdir -p "$dst"
    else
      [ -e "$dst" ] || : > "$dst"
    fi
    if ! findmnt -rn -M "$dst" >/dev/null 2>&1; then
      mount --bind "$host_path" "$dst"
    fi
    printf '%s\n' "$dst" >> "$SESSION_DIR/device_binds"
  done
}

mount_extra_binds() {
  [ -n "${GEMA_EXTRA_BINDS:-}" ] || return 0
  : > "$SESSION_DIR/extra_binds"
  IFS=',' read -r -a binds <<< "$GEMA_EXTRA_BINDS"
  for binding in "${binds[@]}"; do
    [ -n "$binding" ] || continue
    local host_path guest_path dst
    host_path="${binding%%:*}"
    guest_path="${binding#*:}"
    [ "$guest_path" != "$binding" ] || { echo "ERROR: extra bind requires host:guest" >&2; exit 66; }
    case "$guest_path" in
      /home/gema/external/*|/mnt/gema/*) ;;
      *) echo "ERROR: extra bind guest path is restricted: $guest_path" >&2; exit 66 ;;
    esac
    [ -e "$host_path" ] || { echo "ERROR: extra bind source missing: $host_path" >&2; exit 66; }
    dst="$ROOTFS$guest_path"
    mkdir -p "$dst"
    if ! findmnt -rn -M "$dst" >/dev/null 2>&1; then
      mount --rbind "$host_path" "$dst"
    fi
    printf '%s\n' "$dst" >> "$SESSION_DIR/extra_binds"
  done
}

cleanup_session() {
  local id="$1"
  if [ ! -d "$ROOT/sessions/$id" ] && [ ! -d "/sys/fs/cgroup/gema-system/$id" ]; then
    cleanup_network "$id"
    return 0
  fi
  if [ -d "/sys/fs/cgroup/gema-system/$id" ]; then
    if [ -f "/sys/fs/cgroup/gema-system/$id/cgroup.procs" ]; then
      while read -r pid; do
        [ -n "$pid" ] && kill -TERM "$pid" >/dev/null 2>&1 || true
      done < "/sys/fs/cgroup/gema-system/$id/cgroup.procs"
    fi
    if ip netns list 2>/dev/null | grep -q "^gema-$id"; then
      for pid in $(ip netns pids "gema-$id" 2>/dev/null || true); do
        [ -n "$pid" ] && kill -TERM "$pid" >/dev/null 2>&1 || true
      done
    fi
    sleep 1
    if [ -f "/sys/fs/cgroup/gema-system/$id/cgroup.procs" ]; then
      while read -r pid; do
        [ -n "$pid" ] && kill -KILL "$pid" >/dev/null 2>&1 || true
      done < "/sys/fs/cgroup/gema-system/$id/cgroup.procs"
    fi
    if ip netns list 2>/dev/null | grep -q "^gema-$id"; then
      for pid in $(ip netns pids "gema-$id" 2>/dev/null || true); do
        [ -n "$pid" ] && kill -KILL "$pid" >/dev/null 2>&1 || true
      done
    fi
  fi
  if [ -f "$ROOT/sessions/$id/router.pids" ]; then
    while read -r pid; do
      [ -n "$pid" ] && kill "$pid" >/dev/null 2>&1 || true
    done < "$ROOT/sessions/$id/router.pids"
  fi
  cleanup_mounts "$id"
  cleanup_network "$id"
  if [ "$PRESERVE_SESSION" = "1" ]; then
    rm -f "$ROOT/sessions/$id/router.pids"
  else
    rm -rf "$ROOT/sessions/$id"
  fi
}

session_net_parts() {
  python3 - "${1:-$SESSION_ID}" <<'PY'
import hashlib, sys
digest = hashlib.sha256(sys.argv[1].encode()).hexdigest()
value = int(digest[:4], 16)
octet2 = 64 + ((value >> 8) % 64)
octet3 = value & 255
print(f"{digest[:10]} {octet2} {octet3}")
PY
}

delete_nat_rule() {
  local subnet="$1"
  [ -n "$subnet" ] || return 0
  while iptables -t nat -C POSTROUTING -s "$subnet" -j MASQUERADE >/dev/null 2>&1; do
    iptables -t nat -D POSTROUTING -s "$subnet" -j MASQUERADE >/dev/null 2>&1 || break
  done
}

delete_forward_rules() {
  local subnet="$1"
  [ -n "$subnet" ] || return 0
  while iptables -C FORWARD -s "$subnet" -j ACCEPT >/dev/null 2>&1; do
    iptables -D FORWARD -s "$subnet" -j ACCEPT >/dev/null 2>&1 || break
  done
  while iptables -C FORWARD -d "$subnet" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT >/dev/null 2>&1; do
    iptables -D FORWARD -d "$subnet" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT >/dev/null 2>&1 || break
  done
}

subnet_in_use_by_other_session() {
  local subnet="$1"
  python3 - "$ROOT" "$SESSION_ID" "$subnet" <<'PY'
import json, pathlib, sys
root, current, subnet = sys.argv[1:]
sessions = pathlib.Path(root) / "sessions"
for path in sessions.glob("*/network.json"):
    if path.parent.name == current:
        continue
    try:
        data = json.loads(path.read_text())
    except Exception:
        continue
    if data.get("subnet") == subnet:
        print(path.parent.name)
        sys.exit(0)
sys.exit(1)
PY
}

prepare_network() {
  [ "$NETWORK" = "1" ] || return 0
  local net_hash octet2 octet3 host_ip guest_ip subnet host_veth guest_veth
  read -r net_hash octet2 octet3 < <(session_net_parts)
  host_ip="10.${octet2}.${octet3}.1"
  guest_ip="10.${octet2}.${octet3}.2"
  subnet="10.${octet2}.${octet3}.0/24"
  host_veth="gvh${net_hash}"
  guest_veth="gvg${net_hash}"

  if other_session="$(subnet_in_use_by_other_session "$subnet")"; then
    echo "ERROR: session network subnet collision: $subnet already used by $other_session" >&2
    exit 70
  fi
  delete_nat_rule "$subnet"
  delete_forward_rules "$subnet"
  ip netns del "$NETNS" >/dev/null 2>&1 || true
  ip link del "$host_veth" >/dev/null 2>&1 || true
  ip netns add "$NETNS"
  ip link add "$host_veth" type veth peer name "$guest_veth"
  ip link set "$guest_veth" netns "$NETNS"
  ip addr add "${host_ip}/24" dev "$host_veth"
  ip link set "$host_veth" up
  ip netns exec "$NETNS" ip addr add "${guest_ip}/24" dev "$guest_veth"
  ip netns exec "$NETNS" ip link set "$guest_veth" up
  ip netns exec "$NETNS" ip link set lo up
  ip netns exec "$NETNS" ip route add default via "$host_ip"
  sysctl -w net.ipv4.ip_forward=1 >/dev/null
  iptables -t nat -C POSTROUTING -s "$subnet" -j MASQUERADE >/dev/null 2>&1 \
    || iptables -t nat -A POSTROUTING -s "$subnet" -j MASQUERADE
  iptables -C FORWARD -d "$subnet" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT >/dev/null 2>&1 \
    || iptables -I FORWARD 1 -d "$subnet" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
  iptables -C FORWARD -s "$subnet" -j ACCEPT >/dev/null 2>&1 \
    || iptables -I FORWARD 1 -s "$subnet" -j ACCEPT

  cat > "$SESSION_DIR/network.json" <<EOF
{
  "netns": "$NETNS",
  "host_veth": "$host_veth",
  "guest_veth": "$guest_veth",
  "host_ip": "$host_ip",
  "guest_ip": "$guest_ip",
  "subnet": "$subnet",
  "nat_rule": {
    "table": "nat",
    "chain": "POSTROUTING",
    "source": "$subnet",
    "target": "MASQUERADE"
  },
  "forward_rules": [
    {
      "chain": "FORWARD",
      "source": "$subnet",
      "target": "ACCEPT"
    },
    {
      "chain": "FORWARD",
      "destination": "$subnet",
      "ctstate": "RELATED,ESTABLISHED",
      "target": "ACCEPT"
    }
  ]
}
EOF
}

cleanup_network() {
  local id="$1"
  local netns="gema-$id"
  local network_file="$ROOT/sessions/$id/network.json"
  local host_veth=""
  local subnet=""
  if [ -f "$network_file" ]; then
    host_veth="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("host_veth",""))' "$network_file" 2>/dev/null || true)"
    subnet="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("subnet",""))' "$network_file" 2>/dev/null || true)"
  else
    read -r net_hash octet2 octet3 < <(session_net_parts "$id")
    subnet="10.${octet2}.${octet3}.0/24"
    host_veth="gvh${net_hash}"
  fi
  delete_nat_rule "$subnet"
  delete_forward_rules "$subnet"
  [ -n "$host_veth" ] && ip link del "$host_veth" >/dev/null 2>&1 || true
  ip netns del "$netns" >/dev/null 2>&1 || true
}

start_port_router() {
  [ -n "$PORT_MAPS" ] || return 0
  local guest_ip
  guest_ip="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["guest_ip"])' "$SESSION_DIR/network.json")"
  : > "$SESSION_DIR/ports.jsonl"
  IFS=',' read -r -a maps <<< "$PORT_MAPS"
  for mapping in "${maps[@]}"; do
    [ -n "$mapping" ] || continue
    local left proto host_port guest_port router_pid
    left="${mapping%/*}"
    proto="${mapping##*/}"
    [ "$left" != "$proto" ] || proto="tcp"
    host_port="${left%%:*}"
    guest_port="${left##*:}"
    if [ "$proto" != "tcp" ] && [ "$proto" != "udp" ]; then
      echo "ERROR: native port router supports tcp and udp only: $mapping" >&2
      exit 65
    fi
    if [ ! -x "$ROUTER_BIN" ]; then
      echo "ERROR: router binary is missing or not executable: $ROUTER_BIN" >&2
      exit 65
    fi
    "$ROUTER_BIN" hostd router "$proto" "$host_port" "$guest_ip" "$guest_port" \
      > "$SESSION_DIR/logs/router-${host_port}-${guest_port}.log" 2>&1 &
    router_pid=$!
    echo "$router_pid" >> "$SESSION_DIR/router.pids"
    printf '{"protocol":"%s","host_port":%s,"guest_ip":"%s","guest_port":%s,"pid":%s,"router":"gema-runtime"}\n' \
      "$proto" "$host_port" "$guest_ip" "$guest_port" "$router_pid" >> "$SESSION_DIR/ports.jsonl"
  done
}

start_workspace_mount_watchdog() {
  local require_storage="${GEMA_REQUIRE_WORKSPACE_STORAGE:-}"
  if [ -z "$require_storage" ]; then
    if [ "$USER_ID" != "local" ] || [ -n "${GEMA_R2_WORKSPACE_REMOTE:-}" ]; then
      require_storage="1"
    else
      require_storage="0"
    fi
  fi
  [ "$require_storage" = "1" ] || return 0
  [ -n "${WORKSPACE:-}" ] || return 0
  (
    local failures=0
    while kill -0 "$child" >/dev/null 2>&1; do
      if ! is_rclone_mount "$WORKSPACE" || ! timeout 3 ls "$WORKSPACE" >/dev/null 2>&1; then
        failures=$((failures + 1))
        echo "[gema-substrate] workspace R2 mount unhealthy attempt=$failures session=$SESSION_ID" >&2
        mount_host_r2_workspace >/dev/null 2>&1 || true
        if [ "$failures" -ge "${GEMA_WORKSPACE_MOUNT_WATCHDOG_FAILURES:-5}" ]; then
          echo "[gema-substrate] workspace R2 mount remained unhealthy; terminating session=$SESSION_ID" >&2
          kill -TERM "$child" >/dev/null 2>&1 || true
          sleep 2
          kill -KILL "$child" >/dev/null 2>&1 || true
          break
        fi
      else
        failures=0
      fi
      sleep 2
    done
  ) &
  workspace_watchdog=$!
}

network_value() {
  local key="$1"
  local file="$SESSION_DIR/network.json"
  [ -f "$file" ] || return 0
  python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get(sys.argv[2],""))' "$file" "$key"
}

write_metadata() {
  mkdir -p "$SESSION_DIR"
  cat > "$SESSION_DIR/meta.json" <<EOF
{
  "session_id": "$SESSION_ID",
  "user_id": "$USER_ID",
  "rootfs": "$ROOTFS",
  "workspace": "$WORKSPACE",
  "agent_dir": "$AGENT_DIR",
  "cgroup": "$CGROUP",
  "memory_max": "$MEMORY_MAX",
  "cpu_max": "$CPU_MAX",
  "pids_max": "$PIDS_MAX",
  "hardening": "$HARDENING",
  "isolation_lane": "$ISOLATION_LANE",
  "no_new_privs": "$NO_NEW_PRIVS",
  "drop_caps": "$DROP_CAPS",
  "seccomp": "$SECCOMP",
  "apparmor_profile": "$APPARMOR_PROFILE",
  "created_at": "$(date -Is)"
}
EOF
}

hydrate_agent_memory() {
  python3 - "$ROOT" "$SESSION_ID" "$USER_ID" "$AGENT_DIR" <<'PY'
import json
import shutil
import sys
from pathlib import Path

root = Path(sys.argv[1])
session_id = sys.argv[2]
user_id = sys.argv[3]
agent_dir = Path(sys.argv[4])
memory_dir = agent_dir / "memory"
vector_index = memory_dir / "vector" / "index.json"

def vector_count(path: Path) -> int:
    try:
        data = json.loads(path.read_text())
        items = data.get("items")
        return len(items) if isinstance(items, list) else 0
    except Exception:
        return 0

if vector_count(vector_index) > 0:
    raise SystemExit(0)

candidates = []
for meta_path in (root / "sessions").glob("*/meta.json"):
    try:
        meta = json.loads(meta_path.read_text())
    except Exception:
        continue
    if meta.get("session_id") == session_id or meta.get("user_id") != user_id:
        continue
    source_agent = Path(str(meta.get("agent_dir") or ""))
    source_memory = source_agent / "memory"
    source_index = source_memory / "vector" / "index.json"
    count = vector_count(source_index)
    if count <= 0:
        continue
    try:
        mtime = source_index.stat().st_mtime
    except OSError:
        mtime = 0
    candidates.append((mtime, count, source_memory))

if not candidates:
    raise SystemExit(0)

candidates.sort(reverse=True)
source_memory = candidates[0][2]
memory_dir.mkdir(parents=True, exist_ok=True)
for name in ("projects", "vector"):
    src = source_memory / name
    dst = memory_dir / name
    if not src.exists():
        continue
    if dst.exists():
        shutil.rmtree(dst)
    shutil.copytree(src, dst)
PY
}

write_security_state() {
  local status="enabled"
  local setpriv_path=""
  setpriv_path="$(command -v setpriv 2>/dev/null || true)"
  if [ "$HARDENING" = "1" ] && [ -z "$setpriv_path" ]; then
    echo "ERROR: hardening requires setpriv" >&2
    exit 78
  fi
  if [ -n "$APPARMOR_PROFILE" ] && ! aa-status 2>/dev/null | grep -Fxq "   $APPARMOR_PROFILE"; then
    echo "ERROR: AppArmor profile is not loaded: $APPARMOR_PROFILE" >&2
    exit 78
  fi
  [ "$HARDENING" = "1" ] || status="disabled"
  cat > "$SESSION_DIR/security.json" <<EOF
{
  "status": "$status",
  "mode": "native-linux",
  "isolation_lane": "$ISOLATION_LANE",
  "no_new_privs": "$NO_NEW_PRIVS",
  "capability_bounding_set": "$( [ "$DROP_CAPS" = "1" ] && echo dropped || echo inherited )",
  "seccomp": "$( [ "$SECCOMP" = "1" ] && echo filtered || echo disabled )",
  "apparmor_profile": "$( [ -n "$APPARMOR_PROFILE" ] && echo "$APPARMOR_PROFILE" || echo none )",
  "device_policy": "$( [ -f "$SESSION_DIR/device_policy.json" ] && echo cgroup-v2-ebpf || echo minimal-dev-rootfs-plus-explicit-device-binds )",
  "network_namespace": "$NETWORK",
  "cgroup": "$CGROUP",
  "recorded_at": "$(date -Is)"
}
EOF
}

prepare_cgroup() {
  mkdir -p /sys/fs/cgroup/gema-system
  for controller in "+cpu" "+memory" "+pids"; do
    if grep -qw "${controller#+}" /sys/fs/cgroup/cgroup.controllers 2>/dev/null; then
      echo "$controller" > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null || true
    fi
  done
  for controller in "+cpu" "+memory" "+pids"; do
    if grep -qw "${controller#+}" /sys/fs/cgroup/gema-system/cgroup.controllers 2>/dev/null; then
      echo "$controller" > /sys/fs/cgroup/gema-system/cgroup.subtree_control 2>/dev/null || true
    fi
  done
  mkdir -p "$CGROUP"
  [ -w "$CGROUP/memory.max" ] && printf '%s\n' "$MEMORY_MAX" > "$CGROUP/memory.max" || true
  [ -w "$CGROUP/cpu.max" ] && printf '%s\n' "$CPU_MAX" > "$CGROUP/cpu.max" || true
  [ -w "$CGROUP/pids.max" ] && printf '%s\n' "$PIDS_MAX" > "$CGROUP/pids.max" || true
}

attach_device_policy() {
  [ "$HARDENING" = "1" ] || return 0
  [ "$DEVICE_POLICY" = "1" ] || return 0
  if ! command -v clang >/dev/null 2>&1 || ! command -v bpftool >/dev/null 2>&1; then
    echo "ERROR: cgroup device policy requires clang and bpftool" >&2
    exit 78
  fi
  mkdir -p "$SESSION_DIR/bpf" /sys/fs/bpf/gema-system
  local src="$SESSION_DIR/bpf/device.c"
  local obj="$SESSION_DIR/bpf/device.o"
  local pin="/sys/fs/bpf/gema-system/${SESSION_ID}-device"
  rm -f "$pin" "$obj" "$src"
  DEVICE_ALLOWLIST="$DEVICE_ALLOWLIST" python3 - "$src" <<'PY'
import json, os, sys
rules = json.loads(os.environ.get("DEVICE_ALLOWLIST") or "[]")
lines = [
    "#define SEC(NAME) __attribute__((section(NAME), used))",
    "#include <linux/bpf.h>",
    "SEC(\"cgroup/dev\")",
    "int gema_device_policy(struct bpf_cgroup_dev_ctx *ctx) {",
    "  unsigned int type = ctx->access_type & 0xffff;",
    "  unsigned int access = ctx->access_type >> 16;",
    "  if (access & BPF_DEVCG_ACC_MKNOD) return 0;",
]
for rule in rules:
    kind = 2 if rule.get("kind") == "char" else 1
    major = int(rule.get("major", -1))
    minor = rule.get("minor", "*")
    minor_check = "1" if minor == "*" else f"ctx->minor == {int(minor)}"
    access = str(rule.get("access", "rw"))
    access_checks = []
    if "r" not in access:
        access_checks.append("!(access & BPF_DEVCG_ACC_READ)")
    if "w" not in access:
        access_checks.append("!(access & BPF_DEVCG_ACC_WRITE)")
    access_check = " && ".join(access_checks) if access_checks else "1"
    lines.append(f"  if (type == {kind} && ctx->major == {major} && ({minor_check}) && ({access_check})) return 1;")
lines += [
    "  return 0;",
    "}",
    "char _license[] SEC(\"license\") = \"GPL\";",
]
open(sys.argv[1], "w").write("\n".join(lines) + "\n")
PY
  clang -O2 -g -target bpf -I/usr/include/x86_64-linux-gnu -c "$src" -o "$obj"
  bpftool prog load "$obj" "$pin" type cgroup/dev
  bpftool cgroup attach "$CGROUP" cgroup_device pinned "$pin"
  printf '%s\n' "$pin" > "$SESSION_DIR/bpf/device.pin"
  cat > "$SESSION_DIR/device_policy.json" <<EOF
{
  "mode": "cgroup-v2-ebpf",
  "pin": "$pin",
  "object": "$obj",
  "allowlist": $DEVICE_ALLOWLIST,
  "attached_at": "$(date -Iseconds)"
}
EOF
}

is_rclone_mount() {
  local path="$1"
  findmnt -rn -M "$path" -o FSTYPE 2>/dev/null | grep -Eq '(^|[[:space:]])(fuse\.rclone|rclone)($|[[:space:]])'
}

detach_stale_workspace_mount() {
  local path="$1"
  if findmnt -rn -M "$path" >/dev/null 2>&1 && ! timeout 3 ls "$path" >/dev/null 2>&1; then
    fusermount3 -uz "$path" 2>/dev/null || fusermount -uz "$path" 2>/dev/null || umount -l "$path" 2>/dev/null || true
  fi
}

host_workspace_storage_required() {
  [ "${GEMA_REQUIRE_WORKSPACE_STORAGE:-0}" = "1" ] && return 0
  [ "$USER_ID" != "local" ] && return 0
  [ -n "${GEMA_R2_WORKSPACE_REMOTE:-}" ] && return 0
  return 1
}

mount_host_r2_workspace() {
  local remote="${GEMA_R2_WORKSPACE_REMOTE:-}"
  [ -n "$remote" ] || return 1
  command -v rclone >/dev/null 2>&1 || return 1
  mkdir -p "$WORKSPACE"
  if is_rclone_mount "$WORKSPACE" && timeout 3 ls "$WORKSPACE" >/dev/null 2>&1; then
    return 0
  fi
  if findmnt -rn -M "$WORKSPACE" >/dev/null 2>&1; then
    fusermount3 -uz "$WORKSPACE" 2>/dev/null || fusermount -uz "$WORKSPACE" 2>/dev/null || umount -l "$WORKSPACE" 2>/dev/null || true
  fi
  if [ -n "$(find "$WORKSPACE" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)" ]; then
    local quarantine
    quarantine="$(dirname "$WORKSPACE")/workspace.local-fallback.$(date +%s)"
    mkdir -p "$quarantine"
    find "$WORKSPACE" -mindepth 1 -maxdepth 1 -exec mv {} "$quarantine"/ \;
    chown -R root:root "$quarantine"
    find "$quarantine" -type d -exec chmod 0555 {} +
    find "$quarantine" -type f -exec chmod 0444 {} +
  fi
  chown root:root "$WORKSPACE"
  chmod 0555 "$WORKSPACE"
  local cache_root="${GEMA_RCLONE_CACHE_ROOT:-/var/lib/gema-system/rclone-cache}"
  local cache_dir="$cache_root/$SESSION_ID/workspace"
  mkdir -p "$SESSION_DIR/logs" "$cache_dir"
  rclone mkdir "$remote" >/dev/null 2>&1 || true
  rclone mount "$remote" "$WORKSPACE" \
    --vfs-cache-mode full \
    --vfs-cache-max-size "${RCLONE_VFS_CACHE_SIZE:-12G}" \
    --vfs-cache-max-age "${RCLONE_VFS_CACHE_MAX_AGE:-6h}" \
    --vfs-write-back "${RCLONE_VFS_WRITE_BACK:-5s}" \
    --dir-cache-time "${RCLONE_DIR_CACHE_TIME:-30s}" \
    --attr-timeout "${RCLONE_ATTR_TIMEOUT:-1s}" \
    --poll-interval 0 \
    --umask 007 \
    --file-perms 0770 \
    --dir-perms 0770 \
    --uid "$GUEST_UID" \
    --gid "$GUEST_GID" \
    --allow-other \
    --links \
    --cache-dir "$cache_dir" \
    --log-file "$SESSION_DIR/logs/r2-mount-workspace.log" \
    --log-level "${RCLONE_MOUNT_LOG_LEVEL:-INFO}" \
    --daemon
  for _ in $(seq 1 40); do
    if is_rclone_mount "$WORKSPACE" && timeout 3 ls "$WORKSPACE" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.25
  done
  return 1
}

ensure_host_workspace_storage() {
  if host_workspace_storage_required; then
    if ! is_rclone_mount "$WORKSPACE" || ! timeout 3 ls "$WORKSPACE" >/dev/null 2>&1; then
      mount_host_r2_workspace || true
    fi
    if ! is_rclone_mount "$WORKSPACE" || ! timeout 3 ls "$WORKSPACE" >/dev/null 2>&1; then
      echo "ERROR: host workspace storage is not mounted through healthy R2; refusing local bind" >&2
      exit 70
    fi
  fi
}

bind_workspace_into_rootfs() {
  mkdir -p "$ROOTFS/home/gema/workspace"
  chown "$GUEST_UID:$GUEST_GID" "$ROOTFS/home/gema" "$ROOTFS/home/gema/workspace" 2>/dev/null || true
  chmod 0750 "$ROOTFS/home/gema" "$ROOTFS/home/gema/workspace" 2>/dev/null || true
  if findmnt -rn -M "$ROOTFS/home/gema/workspace" >/dev/null 2>&1; then
    umount -l "$ROOTFS/home/gema/workspace" || true
  fi
  mount --rbind "$WORKSPACE" "$ROOTFS/home/gema/workspace"
}

mount_runtime_apt_tmpfs() {
  local dst="$ROOTFS/run/gema-system/apt"
  mkdir -p "$dst"
  if ! findmnt -rn -M "$dst" >/dev/null 2>&1; then
    mount -t tmpfs -o nosuid,nodev,mode=1777,size="${GEMA_APT_TMPFS_SIZE:-2G}" tmpfs "$dst"
  fi
  chown "$GUEST_UID:$GUEST_GID" "$dst" 2>/dev/null || true
}

mount_runtime_r2_workspace() {
  local remote="${GEMA_R2_WORKSPACE_REMOTE:-}"
  [ -n "$remote" ] || return 0
  command -v rclone >/dev/null 2>&1 || { echo "ERROR: rclone missing; cannot mount R2 workspace" >&2; exit 78; }
  command -v fusermount3 >/dev/null 2>&1 || command -v fusermount >/dev/null 2>&1 || { echo "ERROR: FUSE helper missing; cannot mount R2 workspace" >&2; exit 78; }
  mkdir -p /home/gema/workspace /home/gema/.cache/rclone /home/gema/.config/rclone
  if findmnt -rn -M /home/gema/workspace >/dev/null 2>&1 && timeout 3 ls /home/gema/workspace >/dev/null 2>&1; then
    return 0
  fi
  chmod 666 /dev/fuse 2>/dev/null || true
  if [ -w /etc/fuse.conf ] && ! grep -q '^user_allow_other' /etc/fuse.conf 2>/dev/null; then
    printf '\nuser_allow_other\n' >> /etc/fuse.conf || true
  fi
  if is_rclone_mount /home/gema/workspace; then
    if timeout 3 ls /home/gema/workspace >/dev/null 2>&1; then
      return 0
    fi
    fusermount3 -uz /home/gema/workspace 2>/dev/null || fusermount -uz /home/gema/workspace 2>/dev/null || umount -l /home/gema/workspace 2>/dev/null || true
  fi
  rclone mkdir "$remote" >/dev/null 2>&1 || true
  local cache_root="${GEMA_RCLONE_CACHE_ROOT:-/var/lib/gema-system/rclone-cache}"
  local cache_dir="$cache_root/$SESSION_ID/workspace"
  mkdir -p "$cache_dir"
  rclone mount "$remote" /home/gema/workspace \
    --vfs-cache-mode full \
    --vfs-cache-max-size "${RCLONE_VFS_CACHE_SIZE:-12G}" \
    --vfs-cache-max-age "${RCLONE_VFS_CACHE_MAX_AGE:-6h}" \
    --vfs-write-back "${RCLONE_VFS_WRITE_BACK:-5s}" \
    --dir-cache-time "${RCLONE_DIR_CACHE_TIME:-30s}" \
    --attr-timeout "${RCLONE_ATTR_TIMEOUT:-1s}" \
    --poll-interval 0 \
    --umask 007 \
    --file-perms 0770 \
    --dir-perms 0770 \
    --uid "$GUEST_UID" \
    --gid "$GUEST_GID" \
    --allow-other \
    --links \
    --cache-dir "$cache_dir" \
    --allow-non-empty \
    --log-file /tmp/rclone-workspace-mount.log \
    --log-level "${RCLONE_MOUNT_LOG_LEVEL:-INFO}" \
    --daemon
  for _ in $(seq 1 30); do
    if is_rclone_mount /home/gema/workspace && timeout 3 ls /home/gema/workspace >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "ERROR: R2 workspace mount did not become healthy" >&2
  tail -n 80 /tmp/rclone-workspace-mount.log 2>/dev/null || true
  exit 70
}

prepare_workspace_user_state() {
  local workspace="/home/gema/workspace"
  [ -d "$workspace" ] || return 0
  if ! findmnt -rn -M "$workspace" >/dev/null 2>&1 || ! timeout 3 ls "$workspace" >/dev/null 2>&1; then
    echo "ERROR: workspace storage is not a healthy mounted volume; refusing local user state" >&2
    exit 70
  fi
  mkdir -p \
    "$workspace/.gema/tmp" \
    "$workspace/.gema/cache/pip" \
    "$workspace/.gema/cache/xdg" \
    "$workspace/.gema/cache/npm" \
    "$workspace/.gema/cache/yarn" \
    "$workspace/.gema/cache/huggingface/transformers" \
    "$workspace/.gema/cache/torch" \
    "$workspace/.gema/node" \
    "$workspace/.gema/apt/root" \
    "$workspace/.gema/apt/cache/archives/partial" \
    "$workspace/.gema/apt/lists/partial" \
    "$workspace/.gema/apt/state" \
    "$workspace/.gema/env" \
    "$workspace/.gema/home/.cache" \
    "$workspace/.gema/home/.config" \
    "$workspace/.gema/home/.local" \
    "$workspace/.gema/home/.npm" \
    "$workspace/.gema/home/.cargo" \
    "$workspace/.gema/home/.rustup" \
    "$workspace/.gema/home/.ssh"
  find "$workspace/.gema" -maxdepth 2 -type d -exec chown "$GUEST_UID:$GUEST_GID" {} + 2>/dev/null || true
  chmod 700 "$workspace/.gema/home/.ssh" 2>/dev/null || true

  local name src dst
  for name in .cache .config .local .npm .cargo .rustup .ssh; do
    src="$workspace/.gema/home/$name"
    dst="/home/gema/$name"
    mkdir -p "$src"
    if findmnt -rn -M "$dst" >/dev/null 2>&1; then
      continue
    fi
    if [ -d "$dst" ]; then
      cp -a "$dst"/. "$src"/ 2>/dev/null || true
    else
      rm -f "$dst" 2>/dev/null || true
      mkdir -p "$dst"
    fi
    mount --bind "$src" "$dst" 2>/dev/null || {
      rm -rf "$dst" 2>/dev/null || true
      ln -s "$src" "$dst" 2>/dev/null || true
    }
    chown "$GUEST_UID:$GUEST_GID" "$src" "$dst" 2>/dev/null || true
  done
}

run_substrate() {
  require_root
  validate_session_id "$SESSION_ID"
  shift
  [ "${1:-}" = "--" ] && shift
  if [ "$#" = "0" ]; then
    set -- /bin/bash -lc 'id && pwd && ls -la /home/gema/workspace'
  fi

  ensure_rootfs
  if [ ! -x "$ROOTFS/usr/bin/setpriv" ]; then
    echo "ERROR: launch requires /usr/bin/setpriv inside the rootfs" >&2
    exit 78
  fi
  cleanup_session "$SESSION_ID"
  detach_stale_workspace_mount "$WORKSPACE"
  mkdir -p "$WORKSPACE" "$SESSION_DIR/logs"
  write_metadata
  hydrate_agent_memory
  if [ -f /root/.config/rclone/rclone.conf ]; then
    mkdir -p "$ROOTFS/home/gema/.config/rclone" "$ROOTFS/home/gema/.cache/rclone"
    install -m 600 /root/.config/rclone/rclone.conf "$ROOTFS/home/gema/.config/rclone/rclone.conf"
    find "$ROOTFS/home/gema/.config" "$ROOTFS/home/gema/.cache" -maxdepth 2 -type d -exec chown "$GUEST_UID:$GUEST_GID" {} + 2>/dev/null || true
    chown "$GUEST_UID:$GUEST_GID" "$ROOTFS/home/gema/.config/rclone/rclone.conf" 2>/dev/null || true
  fi
  if ! is_rclone_mount "$WORKSPACE"; then
    chown "$GUEST_UID:$GUEST_GID" "$WORKSPACE" 2>/dev/null || true
  fi
  prepare_cgroup
  attach_device_policy
  write_security_state
  prepare_network

  ensure_host_workspace_storage
  prepare_guest_identity
  mkdir -p "$ROOTFS/home/gema" "$ROOTFS/home/gema/workspace"
  chown "$GUEST_UID:$GUEST_GID" "$ROOTFS/home/gema" "$ROOTFS/home/gema/workspace" 2>/dev/null || true
  chmod 0750 "$ROOTFS/home/gema" "$ROOTFS/home/gema/workspace" 2>/dev/null || true
  mkdir -p "$AGENT_DIR/snapshots"
  find "$AGENT_DIR" -maxdepth 2 -type d -exec chown "$GUEST_UID:$GUEST_GID" {} + 2>/dev/null || true
  bind_workspace_into_rootfs
  mkdir -p "$ROOTFS/home/gema/.gema/agent"
  chown "$GUEST_UID:$GUEST_GID" "$ROOTFS/home/gema/.gema" "$ROOTFS/home/gema/.gema/agent" 2>/dev/null || true
  mount_if_needed "$AGENT_DIR" "$ROOTFS/home/gema/.gema/agent" bind
  mount_if_needed tmpfs "$ROOTFS/tmp" tmpfs
  mount_runtime_apt_tmpfs
  mount_extra_binds
  runtime_hostname="$(session_hostname)"
  write_runtime_identity "$runtime_hostname"
  if [ "$HARDENING" = "1" ] && [ "$SECCOMP" = "1" ]; then
    install_file_if_changed "$ROUTER_BIN" "$ROOTFS/usr/local/bin/gema-runtime"
  fi
  mount_device_binds

  printf '[gema-substrate] session=%s rootfs=%s workspace=%s\n' "$SESSION_ID" "$ROOTFS" "$WORKSPACE"
  set +e
  ns_prefix=()
  host_ip=""
  guest_ip=""
  if [ "$NETWORK" = "1" ]; then
    ns_prefix=(ip netns exec "$NETNS")
    host_ip="$(network_value host_ip)"
    guest_ip="$(network_value guest_ip)"
  fi
  launcher=("${ns_prefix[@]}")
  if [ "$HARDENING" = "1" ] && [ -n "$APPARMOR_PROFILE" ]; then
    launcher+=(setpriv --apparmor-profile "$APPARMOR_PROFILE")
  fi
  guest_priv=(/usr/bin/setpriv --reuid "$GUEST_UID" --regid "$GUEST_GID" --clear-groups)
  if [ "$HARDENING" = "1" ]; then
    [ "$NO_NEW_PRIVS" = "1" ] && guest_priv+=(--nnp)
    if [ "$DROP_CAPS" = "1" ]; then
      guest_priv+=(--bounding-set=-all --inh-caps=-all --ambient-caps=-all)
    fi
  fi
  guest_priv+=(--)
  seccomp_exec=()
  if [ "$HARDENING" = "1" ] && [ "$SECCOMP" = "1" ]; then
    seccomp_exec=(/usr/local/bin/gema-runtime hostd seccomp-exec --)
  fi
  export -f is_rclone_mount mount_runtime_r2_workspace
  export -f prepare_workspace_user_state
  "${launcher[@]}" unshare --fork --pid --ipc --uts --mount --propagation private \
    /bin/bash -c '
      set -euo pipefail
      rootfs="$1"
      runtime_hostname="$2"
      shift
      shift
      hostname "$runtime_hostname"
      mount --make-rprivate /
      mount --rbind "$rootfs" "$rootfs"
      mkdir -p "$rootfs/.gema-oldroot" "$rootfs/proc"
      cd "$rootfs"
      pivot_root . .gema-oldroot
      mount -t proc proc /proc
      mkdir -p /dev/pts
      if findmnt -rn -M /dev/pts >/dev/null 2>&1; then
        mount -o remount,ptmxmode=0666 /dev/pts >/dev/null 2>&1 || true
      else
        mount -t devpts devpts /dev/pts -o nosuid,noexec,gid=5,mode=620,ptmxmode=0666
      fi
      umount -l /.gema-oldroot
      rmdir /.gema-oldroot >/dev/null 2>&1 || true
      if [ "${GEMA_SSHD_ENABLED:-0}" = "1" ]; then
        if [ ! -x /usr/sbin/sshd ]; then
          echo "ERROR: SSH access requested but /usr/sbin/sshd is missing from rootfs" >&2
          exit 78
        fi
        mkdir -p /run/sshd /etc/ssh /home/gema/.ssh
        if grep -q "^gema:!" /etc/shadow 2>/dev/null; then
          sed -i "s/^gema:![^:]*:/gema:*:/" /etc/shadow
        fi
        printf "%s\n" "${GEMA_AUTHORIZED_KEYS:-}" | sed "/^[[:space:]]*$/d" > /home/gema/.ssh/authorized_keys
        chown "$GUEST_UID:$GUEST_GID" /home/gema/.ssh /home/gema/.ssh/authorized_keys
        chmod 700 /home/gema/.ssh
        chmod 600 /home/gema/.ssh/authorized_keys
        [ -f /etc/ssh/ssh_host_ed25519_key ] || ssh-keygen -q -t ed25519 -N "" -f /etc/ssh/ssh_host_ed25519_key
        [ -f /etc/ssh/ssh_host_rsa_key ] || ssh-keygen -q -t rsa -b 3072 -N "" -f /etc/ssh/ssh_host_rsa_key
        cat > /tmp/gema-sshd_config <<EOF
Port 22
ListenAddress 0.0.0.0
Protocol 2
HostKey /etc/ssh/ssh_host_ed25519_key
HostKey /etc/ssh/ssh_host_rsa_key
PidFile /run/sshd/gema.pid
AuthorizedKeysFile .ssh/authorized_keys
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
ChallengeResponseAuthentication no
PermitRootLogin no
AllowUsers gema
UsePAM no
PermitUserEnvironment no
X11Forwarding no
AllowTcpForwarding yes
GatewayPorts no
PermitTunnel no
LogLevel VERBOSE
Subsystem sftp internal-sftp
EOF
        /usr/sbin/sshd -f /tmp/gema-sshd_config
      fi
      mount_runtime_r2_workspace
      prepare_workspace_user_state
      exec "$@"
    ' gema-pivot "$ROOTFS" "$runtime_hostname" \
    /usr/bin/env -i \
      HOME=/home/gema \
      USER=gema \
      GEMA_REQUIRE_WORKSPACE_STORAGE=1 \
      GEMA_PYTHON_ENV=/home/gema/workspace/.gema/env/python-auto \
      VIRTUAL_ENV=/home/gema/workspace/.gema/env/python-auto \
      PATH=/opt/gema/bin:/home/gema/workspace/.gema/env/python-auto/bin:/home/gema/workspace/.gema/apt/root/usr/local/sbin:/home/gema/workspace/.gema/apt/root/usr/local/bin:/home/gema/workspace/.gema/apt/root/usr/sbin:/home/gema/workspace/.gema/apt/root/usr/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
      TMPDIR=/home/gema/workspace/.gema/tmp \
      PIP_CACHE_DIR=/home/gema/workspace/.gema/cache/pip \
      XDG_CACHE_HOME=/home/gema/workspace/.gema/cache/xdg \
      NPM_CONFIG_CACHE=/home/gema/workspace/.gema/cache/npm \
      NPM_CONFIG_PREFIX=/home/gema/workspace/.gema/node \
      YARN_CACHE_FOLDER=/home/gema/workspace/.gema/cache/yarn \
      CARGO_HOME=/home/gema/workspace/.gema/home/.cargo \
      RUSTUP_HOME=/home/gema/workspace/.gema/home/.rustup \
      HF_HOME=/home/gema/workspace/.gema/cache/huggingface \
      TRANSFORMERS_CACHE=/home/gema/workspace/.gema/cache/huggingface/transformers \
      TORCH_HOME=/home/gema/workspace/.gema/cache/torch \
      GEMA_APT_PREFIX=/home/gema/workspace/.gema/apt \
      LD_LIBRARY_PATH=/home/gema/workspace/.gema/apt/root/usr/local/lib:/home/gema/workspace/.gema/apt/root/usr/lib:/home/gema/workspace/.gema/apt/root/usr/lib/x86_64-linux-gnu:/home/gema/workspace/.gema/apt/root/lib:/home/gema/workspace/.gema/apt/root/lib/x86_64-linux-gnu \
      PKG_CONFIG_PATH=/home/gema/workspace/.gema/apt/root/usr/local/lib/pkgconfig:/home/gema/workspace/.gema/apt/root/usr/lib/pkgconfig:/home/gema/workspace/.gema/apt/root/usr/lib/x86_64-linux-gnu/pkgconfig \
      CMAKE_PREFIX_PATH=/home/gema/workspace/.gema/apt/root/usr:/home/gema/workspace/.gema/apt/root/usr/local \
      CPATH=/home/gema/workspace/.gema/apt/root/usr/include:/home/gema/workspace/.gema/apt/root/usr/include/x86_64-linux-gnu \
      GEMA_NATIVE_RUNTIME=1 \
      GEMA_WORKSPACE_ROOT=/home/gema \
      GEMA_PROJECT_DIR=/home/gema/workspace \
      GEMA_AGENT_DIR=/home/gema/workspace/.gema/agent \
      GEMA_R2_WORKSPACE_REMOTE="${GEMA_R2_WORKSPACE_REMOTE:-}" \
      RCLONE_CONFIG=/home/gema/.config/rclone/rclone.conf \
      GEMA_NETWORK="$NETWORK" \
      GEMA_HOST_IP="$host_ip" \
      GEMA_GUEST_IP="$guest_ip" \
      GEMA_GPU_VENDOR="${GEMA_GPU_VENDOR:-}" \
      GEMA_GPU_DEVICE_ID="${GEMA_GPU_DEVICE_ID:-}" \
      GEMA_HOST_CUDA_VERSION="${GEMA_HOST_CUDA_VERSION:-}" \
      GEMA_CUDA_LANE="${GEMA_CUDA_LANE:-auto}" \
      CUDA_VISIBLE_DEVICES="${CUDA_VISIBLE_DEVICES:-}" \
      SESSION_ID="$SESSION_ID" \
      USER_ID="$USER_ID" \
      /usr/bin/setsid "${guest_priv[@]}" "${seccomp_exec[@]}" /bin/bash -lc 'cd /home/gema/workspace && mkdir -p .gema/logs && exec </dev/null >>.gema/logs/workload.log 2>&1 && exec "$@"' gema-substrate "$@" &
  child=$!
  start_port_router
  workspace_watchdog=""
  start_workspace_mount_watchdog
  (
    while kill -0 "$child" >/dev/null 2>&1; do
      for pid in $(descendants_of "$child"); do
        if [ "$(pid_uid "$pid")" != "0" ]; then
          echo "$pid" > "$CGROUP/cgroup.procs" 2>/dev/null || true
        fi
      done
      sleep 0.2
    done
  ) &
  tracker=$!
  wait "$child"
  status=$?
  kill "$tracker" >/dev/null 2>&1 || true
  [ -n "${workspace_watchdog:-}" ] && kill "$workspace_watchdog" >/dev/null 2>&1 || true
  set -e

  if [ "$KEEP" != "1" ]; then
    cleanup_session "$SESSION_ID"
  else
    echo "[gema-substrate] kept session at $SESSION_DIR"
  fi
  exit "$status"
}

inspect_session() {
  local id="$1"
  validate_session_id "$id"
  local base="$ROOT/sessions/$id"
  local cg="/sys/fs/cgroup/gema-system/$id"
  [ -f "$base/meta.json" ] && cat "$base/meta.json" || echo "{}"
  [ -f "$base/network.json" ] && { echo "-- network --"; cat "$base/network.json"; }
  [ -f "$base/ports.jsonl" ] && { echo "-- ports --"; cat "$base/ports.jsonl"; }
  if [ -d "$cg" ]; then
    echo "-- cgroup.procs --"
    cat "$cg/cgroup.procs" || true
    echo "-- ps --"
    while read -r pid; do
      [ -n "$pid" ] && ps -o pid,ppid,stat,etime,%cpu,%mem,cmd -p "$pid" || true
    done < "$cg/cgroup.procs"
  fi
}

descendants_of() {
  local root="$1"
  local children
  children="$(pgrep -P "$root" 2>/dev/null || true)"
  for child in $children; do
    printf '%s\n' "$child"
    descendants_of "$child"
  done
}

pid_uid() {
  local pid="$1"
  awk '/^Uid:/ {print $2; exit}' "/proc/$pid/status" 2>/dev/null || true
}

case "${1:-}" in
  run) run_substrate "$@" ;;
  inspect)
    [ -n "${2:-}" ] || { usage; exit 64; }
    inspect_session "$2"
    ;;
  cleanup)
    [ -n "${2:-}" ] || { usage; exit 64; }
    require_root
    validate_session_id "$2"
    cleanup_session "$2"
    ;;
  *) usage; exit 64 ;;
esac
