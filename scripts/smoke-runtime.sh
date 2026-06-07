#!/usr/bin/env bash
set -euo pipefail
export PATH="/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:$PATH"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSTEM_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
HOSTD="$SCRIPT_DIR/gema-hostd.sh"
ROOT="${GEMA_SYSTEM_ROOT:-/var/lib/gema-system}"
SMOKE_ROOTFS="${GEMA_SMOKE_ROOTFS:-$ROOT/rootfs/noble-ml-cu121}"
SMOKE_SESSIONS=(
  smoke-http smoke-bpf-deny smoke-security smoke-ml-recovery
  smoke-execution-fabric smoke-restart smoke-udp smoke-failed-launch
  smoke-concurrent-a smoke-concurrent-b smoke-snapshot smoke-r2 smoke-criu
  smoke-process-checkpoint smoke-criu-socket smoke-criu-multiprocess
  smoke-criu-package-rootfs
)

require_root() {
  if [ "$(id -u)" != "0" ]; then
    echo "ERROR: run as root" >&2
    exit 77
  fi
}

section() {
  printf '\n== %s ==\n' "$*"
}

cleanup_session() {
  "$HOSTD" cleanup "$1" >/dev/null 2>&1 || true
}

cleanup_smoke_sessions() {
  if [ "${GEMA_SMOKE_KEEP:-0}" = "1" ]; then
    return
  fi
  local session
  for session in "${SMOKE_SESSIONS[@]}"; do
    cleanup_session "$session"
    "$HOSTD" release-ports "$session" >/dev/null 2>&1 || true
  done
}

trap cleanup_smoke_sessions EXIT

assert_no_port() {
  local port="$1"
  if ss -lntup | grep -q ":$port "; then
    echo "ERROR: port still listening: $port" >&2
    ss -lntup | grep ":$port " >&2 || true
    exit 1
  fi
}

assert_no_cgroup() {
  local id="$1"
  if [ -d "/sys/fs/cgroup/gema-system/$id" ]; then
    echo "ERROR: cgroup still exists: $id" >&2
    exit 1
  fi
}

assert_no_bpf_pin() {
  local id="$1"
  if [ -e "/sys/fs/bpf/gema-system/${id}-device" ]; then
    echo "ERROR: BPF pin still exists: $id" >&2
    exit 1
  fi
}

assert_no_runtime_rootfs_mount() {
  local id="$1"
  if findmnt -rn | grep -Fq "$ROOT/runtime-rootfs/$id/"; then
    echo "ERROR: runtime rootfs mount still exists: $id" >&2
    findmnt -rn | grep -F "$ROOT/runtime-rootfs/$id/" >&2 || true
    exit 1
  fi
}

session_net_subnet() {
  python3 - "$1" <<'PY'
import hashlib, sys
value = int(hashlib.sha256(sys.argv[1].encode()).hexdigest()[:4], 16)
octet2 = 64 + ((value >> 8) % 64)
octet3 = value & 255
print(f"10.{octet2}.{octet3}.0/24")
PY
}

assert_no_nat_subnet() {
  local subnet="$1"
  if iptables -t nat -C POSTROUTING -s "$subnet" -j MASQUERADE >/dev/null 2>&1; then
    echo "ERROR: leaked NAT MASQUERADE rule for $subnet" >&2
    iptables -t nat -S POSTROUTING >&2 || true
    exit 1
  fi
}

assert_no_session_network() {
  local id="$1"
  local subnet
  subnet="$(session_net_subnet "$id")"
  assert_no_nat_subnet "$subnet"
  if ip netns list 2>/dev/null | grep -q "^gema-$id"; then
    echo "ERROR: leaked netns for $id" >&2
    ip netns list >&2 || true
    exit 1
  fi
}

smoke_concurrent_rootfs_isolation() {
  local a="smoke-rootfs-a" b="smoke-rootfs-b"
  local root_a root_b base_a base_b host_a host_b base_host
  cleanup_session "$a"
  cleanup_session "$b"
  "$HOSTD" launch "$a" 20210 8132 tcp --health-path /status.txt --startup-timeout 120 --service-timeout 120 -- \
    /bin/bash -lc 'cd /home/gema/workspace && printf a-ready > status.txt && exec python3 -m http.server 8132 --bind 0.0.0.0' >/tmp/gema-smoke-rootfs-a.json &
  local pid_a=$!
  "$HOSTD" launch "$b" 20211 8133 tcp --health-path /status.txt --startup-timeout 120 --service-timeout 120 -- \
    /bin/bash -lc 'cd /home/gema/workspace && printf b-ready > status.txt && exec python3 -m http.server 8133 --bind 0.0.0.0' >/tmp/gema-smoke-rootfs-b.json &
  local pid_b=$!
  wait "$pid_a"
  wait "$pid_b"
  curl -fsS http://127.0.0.1:20210/status.txt | grep -qx a-ready
  curl -fsS http://127.0.0.1:20211/status.txt | grep -qx b-ready
  root_a="$(jq -r '.runtime_rootfs.rootfs' "$ROOT/sessions/$a/hostd.json")"
  root_b="$(jq -r '.runtime_rootfs.rootfs' "$ROOT/sessions/$b/hostd.json")"
  base_a="$(jq -r '.runtime_rootfs.base_rootfs' "$ROOT/sessions/$a/hostd.json")"
  base_b="$(jq -r '.runtime_rootfs.base_rootfs' "$ROOT/sessions/$b/hostd.json")"
  test "$root_a" != "$root_b"
  test "$base_a" = "$base_b"
  case "$root_a" in "$ROOT/runtime-rootfs/$a/"*) ;; *) echo "ERROR: $a did not use session runtime rootfs: $root_a" >&2; exit 1 ;; esac
  case "$root_b" in "$ROOT/runtime-rootfs/$b/"*) ;; *) echo "ERROR: $b did not use session runtime rootfs: $root_b" >&2; exit 1 ;; esac
  host_a="$(cat "$root_a/etc/hostname")"
  host_b="$(cat "$root_b/etc/hostname")"
  test "$host_a" != "$host_b"
  base_host="$(cat "$base_a/etc/hostname" 2>/dev/null || true)"
  test "$base_host" != "$host_a"
  test "$base_host" != "$host_b"
  cleanup_session "$a"
  cleanup_session "$b"
  assert_no_port 20210
  assert_no_port 20211
  assert_no_cgroup "$a"
  assert_no_cgroup "$b"
  assert_no_runtime_rootfs_mount "$a"
  assert_no_runtime_rootfs_mount "$b"
  assert_no_session_network "$a"
  assert_no_session_network "$b"
}

smoke_failed_launch_network_cleanup() {
  local session="smoke-network-fail"
  local subnet
  cleanup_session "$session"
  subnet="$(session_net_subnet "$session")"
  if "$HOSTD" launch "$session" 20212 8134 tcp --health-path /never-ready --startup-timeout 20 --service-timeout 1 -- \
    /bin/bash -lc 'exit 42' >/tmp/gema-smoke-network-fail.json 2>/tmp/gema-smoke-network-fail.err; then
    echo "ERROR: failed launch unexpectedly succeeded" >&2
    exit 1
  fi
  cleanup_session "$session"
  assert_no_port 20212
  assert_no_cgroup "$session"
  assert_no_runtime_rootfs_mount "$session"
  assert_no_session_network "$session"
  assert_no_nat_subnet "$subnet"
}

smoke_snapshot_runtime_layers() {
  local session="smoke-snapshot-layers"
  local upper state_path diff_path
  cleanup_session "$session"
  "$HOSTD" launch "$session" 20206 8126 tcp --health-path /status.txt --startup-timeout 120 --service-timeout 120 -- \
    /bin/bash -lc 'mkdir -p /home/gema/.local/share/gema && printf runtime-diff-ok > /home/gema/.local/share/gema/runtime-diff.txt && printf workspace-ok > workspace.txt && printf ready > status.txt && exec python3 -m http.server 8126 --bind 0.0.0.0' >/tmp/gema-smoke-snapshot-layers-launch.json
  curl -fsS http://127.0.0.1:20206/status.txt | grep -qx ready
  "$HOSTD" snapshot-create "$session" snap-a >/tmp/gema-smoke-snapshot-layers.json
  jq -e '.ok == true and .snapshot.details.session_state.checksum and .snapshot.details.runtime_diff.included == true' /tmp/gema-smoke-snapshot-layers.json >/dev/null
  state_path="$(jq -r '.snapshot.details.session_state.path' /tmp/gema-smoke-snapshot-layers.json)"
  diff_path="$(jq -r '.snapshot.details.runtime_diff.path' /tmp/gema-smoke-snapshot-layers.json)"
  test -f "$state_path"
  test -f "$diff_path"
  "$HOSTD" stop "$session" >/tmp/gema-smoke-snapshot-layers-stop.json
  upper="$(jq -r '.snapshot.details.runtime_diff.runtime_rootfs.upper' /tmp/gema-smoke-snapshot-layers.json)"
  rm -f "$upper/home/gema/.local/share/gema/runtime-diff.txt"
  test ! -f "$upper/home/gema/.local/share/gema/runtime-diff.txt"
  "$HOSTD" snapshot-restore "$session" snap-a >/tmp/gema-smoke-snapshot-layers-restore.json
  jq -e '.ok == true and .restore.runtime_diff.restored == true' /tmp/gema-smoke-snapshot-layers-restore.json >/dev/null
  test "$(cat "$upper/home/gema/.local/share/gema/runtime-diff.txt")" = "runtime-diff-ok"
  cleanup_session "$session"
  assert_no_port 20206
  assert_no_cgroup "$session"
  assert_no_bpf_pin "$session"
  assert_no_runtime_rootfs_mount "$session"
}

smoke_process_checkpoint() {
  command -v criu >/dev/null || {
    echo "ERROR: criu missing" >&2
    exit 78
  }
  criu check >/tmp/gema-smoke-criu-check.log 2>&1 || {
    echo "ERROR: criu check failed" >&2
    cat /tmp/gema-smoke-criu-check.log >&2
    exit 78
  }
  local session="smoke-process-checkpoint"
  local pid checkpoint_id
  cleanup_session "$session"
  "$HOSTD" launch "$session" auto 8128 tcp --startup-timeout 120 --service-timeout 5 -- \
    /bin/bash -lc 'cd /home/gema/workspace && python3 - <<"PY"
import pathlib, time
pathlib.Path("checkpoint-ready").write_text("ready")
beat = pathlib.Path("checkpoint-heartbeat")
count = 0
while True:
    count += 1
    beat.write_text(str(count))
    time.sleep(1)
PY' >/tmp/gema-smoke-process-checkpoint-launch.json 2>/tmp/gema-smoke-process-checkpoint-launch.err || true
  for _ in $(seq 1 60); do
    if [ -f "$ROOT/workspaces/$session/workspace/checkpoint-ready" ]; then
      break
    fi
    sleep 1
  done
  test -f "$ROOT/workspaces/$session/workspace/checkpoint-ready"
  "$HOSTD" processes "$session" >/tmp/gema-smoke-process-checkpoint-processes.json
  pid="$(jq -r '[.processes.active[] | select(.kind == "workload" and ((.command[0] | tostring | endswith("python3")) or (.command[0] | tostring | endswith("python"))))][0].pid' /tmp/gema-smoke-process-checkpoint-processes.json)"
  test "$pid" != "null"
  checkpoint_id="proc-smoke"
  "$HOSTD" process-checkpoint-create "$session" "$checkpoint_id" "$pid" >/tmp/gema-smoke-process-checkpoint-create.json
  jq -e '.ok == true and .checkpoint.status == "completed" and .checkpoint.bytes > 0' /tmp/gema-smoke-process-checkpoint-create.json >/dev/null
  "$HOSTD" process-checkpoints "$session" | jq -e --arg id "$checkpoint_id" '.ok == true and ([.process_checkpoints.checkpoints[] | select(.id == $id and .status == "completed")] | length) == 1' >/dev/null
  local heartbeat_before heartbeat_after
  heartbeat_before="$(cat "$ROOT/workspaces/$session/workspace/checkpoint-heartbeat")"
  "$HOSTD" process-checkpoint-restore "$session" "$checkpoint_id" terminate-existing >/tmp/gema-smoke-process-checkpoint-restore.json
  jq -e '.ok == true and .policy.terminate_existing == true and (.pid | type == "number")' /tmp/gema-smoke-process-checkpoint-restore.json >/dev/null
  for _ in $(seq 1 20); do
    heartbeat_after="$(cat "$ROOT/workspaces/$session/workspace/checkpoint-heartbeat" 2>/dev/null || echo 0)"
    if [ "$heartbeat_after" -gt "$heartbeat_before" ]; then
      break
    fi
    sleep 1
  done
  test "$heartbeat_after" -gt "$heartbeat_before"
  cleanup_session "$session"
  assert_no_cgroup "$session"
  assert_no_bpf_pin "$session"
  assert_no_runtime_rootfs_mount "$session"
}

smoke_process_checkpoint_extended() {
  command -v criu >/dev/null || {
    echo "ERROR: criu missing" >&2
    exit 78
  }
  criu check >/tmp/gema-smoke-criu-check.log 2>&1 || {
    echo "ERROR: criu check failed" >&2
    cat /tmp/gema-smoke-criu-check.log >&2
    exit 78
  }

  local session pid checkpoint_id before_a before_b after_a after_b rootfs heartbeat_before heartbeat_after

  session="smoke-criu-socket"
  cleanup_session "$session"
  "$HOSTD" launch "$session" 20208 8128 tcp --health-path /status --startup-timeout 120 --service-timeout 120 -- \
    /bin/bash -lc 'cd /home/gema/workspace && python3 - <<"PY"
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import pathlib
hits = pathlib.Path("socket-hits")
hits.write_text("0")
class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"
    def do_GET(self):
        value = int(hits.read_text()) + 1
        hits.write_text(str(value))
        body = b"socket-ok\n"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *_):
        pass
ThreadingHTTPServer(("0.0.0.0", 8128), Handler).serve_forever()
PY' >/tmp/gema-smoke-criu-socket-launch.json
  curl -fsS --max-time 5 http://127.0.0.1:20208/status | grep -qx socket-ok
  "$HOSTD" processes "$session" >/tmp/gema-smoke-criu-socket-processes.json
  pid="$(jq -r '[.processes.active[] | select(.kind == "workload" and ((.command[0] | tostring | endswith("python3")) or (.command[0] | tostring | endswith("python"))))][0].pid' /tmp/gema-smoke-criu-socket-processes.json)"
  test "$pid" != "null"
  "$HOSTD" process-checkpoint-create "$session" socket "$pid" >/tmp/gema-smoke-criu-socket-create.json
  jq -e '.ok == true and .checkpoint.status == "completed"' /tmp/gema-smoke-criu-socket-create.json >/dev/null
  "$HOSTD" process-checkpoint-restore "$session" socket terminate-existing >/tmp/gema-smoke-criu-socket-restore.json
  jq -e '.ok == true and .policy.terminate_existing == true' /tmp/gema-smoke-criu-socket-restore.json >/dev/null
  for _ in $(seq 1 20); do
    if curl -fsS --max-time 5 http://127.0.0.1:20208/status 2>/dev/null | grep -qx socket-ok; then
      break
    fi
    sleep 1
  done
  curl -fsS --max-time 5 http://127.0.0.1:20208/status | grep -qx socket-ok
  cleanup_session "$session"
  assert_no_port 20208
  assert_no_cgroup "$session"
  assert_no_bpf_pin "$session"
  assert_no_runtime_rootfs_mount "$session"

  session="smoke-criu-multiprocess"
  cleanup_session "$session"
  "$HOSTD" launch "$session" auto 8129 tcp --startup-timeout 120 --service-timeout 5 -- \
    /bin/bash -lc 'cd /home/gema/workspace && python3 - <<"PY"
import pathlib, subprocess, sys, time
pathlib.Path("multi-ready").write_text("ready")
code = """
import pathlib
import sys
import time
path = pathlib.Path(sys.argv[1])
count = 0
while True:
    count += 1
    path.write_text(str(count))
    time.sleep(1)
"""
children = [subprocess.Popen([sys.executable, "-c", code, "child-a"]), subprocess.Popen([sys.executable, "-c", code, "child-b"])]
try:
    while True:
        time.sleep(1)
finally:
    for child in children:
        child.terminate()
PY' >/tmp/gema-smoke-criu-multiprocess-launch.json 2>/tmp/gema-smoke-criu-multiprocess-launch.err || true
  for _ in $(seq 1 60); do
    [ -f "$ROOT/workspaces/$session/workspace/child-a" ] && [ -f "$ROOT/workspaces/$session/workspace/child-b" ] && break
    sleep 1
  done
  before_a="$(cat "$ROOT/workspaces/$session/workspace/child-a")"
  before_b="$(cat "$ROOT/workspaces/$session/workspace/child-b")"
  "$HOSTD" processes "$session" >/tmp/gema-smoke-criu-multiprocess-processes.json
  pid="$(jq -r '[.processes.active[] | select(.kind == "workload" and ((.command[0] | tostring | endswith("python3")) or (.command[0] | tostring | endswith("python"))))][0].pid' /tmp/gema-smoke-criu-multiprocess-processes.json)"
  test "$pid" != "null"
  "$HOSTD" process-checkpoint-create "$session" multi "$pid" >/tmp/gema-smoke-criu-multiprocess-create.json
  jq -e '.ok == true and .checkpoint.status == "completed"' /tmp/gema-smoke-criu-multiprocess-create.json >/dev/null
  "$HOSTD" process-checkpoint-restore "$session" multi terminate-existing >/tmp/gema-smoke-criu-multiprocess-restore.json
  jq -e '.ok == true and .policy.terminate_existing == true' /tmp/gema-smoke-criu-multiprocess-restore.json >/dev/null
  for _ in $(seq 1 30); do
    after_a="$(cat "$ROOT/workspaces/$session/workspace/child-a" 2>/dev/null || echo 0)"
    after_b="$(cat "$ROOT/workspaces/$session/workspace/child-b" 2>/dev/null || echo 0)"
    if [ "$after_a" -gt "$before_a" ] && [ "$after_b" -gt "$before_b" ]; then
      break
    fi
    sleep 1
  done
  test "$after_a" -gt "$before_a"
  test "$after_b" -gt "$before_b"
  cleanup_session "$session"
  assert_no_cgroup "$session"
  assert_no_bpf_pin "$session"
  assert_no_runtime_rootfs_mount "$session"

  session="smoke-criu-package-rootfs"
  cleanup_session "$session"
  "$HOSTD" package-install "$session" apt jq >/tmp/gema-smoke-criu-package-install.json
  jq -e '.ok == true and .manager == "apt"' /tmp/gema-smoke-criu-package-install.json >/dev/null
  rootfs="$(jq -r '.runtime_rootfs.rootfs' /tmp/gema-smoke-criu-package-install.json)"
  test -x "$rootfs/usr/bin/jq"
  "$HOSTD" launch "$session" auto 8130 tcp --startup-timeout 120 --service-timeout 5 -- \
    /bin/bash -lc 'cd /home/gema/workspace && jq -n "{package_rootfs:true}" > package-rootfs.json && python3 - <<"PY"
import pathlib, time
pathlib.Path("package-ready").write_text("ready")
beat = pathlib.Path("package-heartbeat")
count = 0
while True:
    count += 1
    beat.write_text(str(count))
    time.sleep(1)
PY' >/tmp/gema-smoke-criu-package-launch.json 2>/tmp/gema-smoke-criu-package-launch.err || true
  for _ in $(seq 1 60); do
    [ -f "$ROOT/workspaces/$session/workspace/package-rootfs.json" ] && [ -f "$ROOT/workspaces/$session/workspace/package-heartbeat" ] && break
    sleep 1
  done
  grep -q package_rootfs "$ROOT/workspaces/$session/workspace/package-rootfs.json"
  "$HOSTD" processes "$session" >/tmp/gema-smoke-criu-package-processes.json
  pid="$(jq -r '[.processes.active[] | select(.kind == "workload" and ((.command[0] | tostring | endswith("python3")) or (.command[0] | tostring | endswith("python"))))][0].pid' /tmp/gema-smoke-criu-package-processes.json)"
  test "$pid" != "null"
  heartbeat_before="$(cat "$ROOT/workspaces/$session/workspace/package-heartbeat")"
  "$HOSTD" process-checkpoint-create "$session" package "$pid" >/tmp/gema-smoke-criu-package-create.json
  jq -e '.ok == true and .checkpoint.status == "completed"' /tmp/gema-smoke-criu-package-create.json >/dev/null
  "$HOSTD" process-checkpoint-restore "$session" package terminate-existing >/tmp/gema-smoke-criu-package-restore.json
  jq -e '.ok == true and .policy.terminate_existing == true' /tmp/gema-smoke-criu-package-restore.json >/dev/null
  for _ in $(seq 1 20); do
    heartbeat_after="$(cat "$ROOT/workspaces/$session/workspace/package-heartbeat" 2>/dev/null || echo 0)"
    if [ "$heartbeat_after" -gt "$heartbeat_before" ]; then
      break
    fi
    sleep 1
  done
  test "$heartbeat_after" -gt "$heartbeat_before"
  cleanup_session "$session"
  assert_no_cgroup "$session"
  assert_no_bpf_pin "$session"
  assert_no_runtime_rootfs_mount "$session"
}

smoke_package_installer() {
  local session="smoke-package-installer"
  local rootfs
  local apt_lock
  cleanup_session "$session"
  if "$HOSTD" package-install "$session" apt openssh-server >/tmp/gema-smoke-package-deny.json 2>/tmp/gema-smoke-package-deny.err; then
    echo "ERROR: blocked apt package was installed" >&2
    exit 1
  fi
  grep -q 'blocked by Gema policy' /tmp/gema-smoke-package-deny.json
  if "$HOSTD" package-install "$session" pip . >/tmp/gema-smoke-package-pip-path.json 2>/tmp/gema-smoke-package-pip-path.err; then
    echo "ERROR: pip path install was accepted" >&2
    exit 1
  fi
  grep -q 'python package rejected by Gema policy' /tmp/gema-smoke-package-pip-path.json
  if "$HOSTD" package-install "$session" npm . >/tmp/gema-smoke-package-npm-path.json 2>/tmp/gema-smoke-package-npm-path.err; then
    echo "ERROR: npm path install was accepted" >&2
    exit 1
  fi
  grep -q 'npm package rejected by Gema policy' /tmp/gema-smoke-package-npm-path.json
  "$HOSTD" package-install "$session" apt jq >/tmp/gema-smoke-package-apt.json
  jq -e '.ok == true and .manager == "apt"' /tmp/gema-smoke-package-apt.json >/dev/null
  rootfs="$(jq -r '.runtime_rootfs.rootfs' /tmp/gema-smoke-package-apt.json)"
  test -x "$rootfs/usr/bin/jq"
  "$HOSTD" package-list "$session" >/tmp/gema-smoke-package-list-apt.json
  jq -e '.ok == true and (.history | length >= 1) and .latest_lock.schema == "gema.package_lock.v1" and ([.latest_lock.apt[] | select(.name == "jq")] | length) == 1' /tmp/gema-smoke-package-list-apt.json >/dev/null
  apt_lock="$(jq -r '.history[-1].lock_path' /tmp/gema-smoke-package-list-apt.json)"
  "$HOSTD" package-verify-lock "$session" "$apt_lock" | jq -e '.ok == true and .schema == "gema.package_verify.v1"' >/dev/null
  "$HOSTD" package-install "$session" python six >/tmp/gema-smoke-package-python.json
  jq -e '.ok == true and .manager == "python"' /tmp/gema-smoke-package-python.json >/dev/null
  chroot "$rootfs" /usr/bin/env -i GEMA_CUDA_LANE=cu121 VIRTUAL_ENV=/opt/gema/ml/cu121 PATH=/opt/gema/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    python -c 'import sys,six; assert sys.prefix == "/opt/gema/ml/cu121"; print(six.__version__)' >/tmp/gema-smoke-package-python-import.txt
  chroot "$rootfs" /usr/bin/env -i GEMA_CUDA_LANE=cu121 VIRTUAL_ENV=/opt/gema/ml/cu121 PATH=/opt/gema/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    pip --version | grep -q '/opt/gema/ml/cu121'
  "$HOSTD" package-list "$session" >/tmp/gema-smoke-package-list-python.json
  jq -e '.latest_lock.schema == "gema.package_lock.v1" and .latest_lock.manager == "python" and ([.latest_lock.python.freeze[] | select(test("^six=="; "i"))] | length) == 1 and (.history[-1].lock_path | type == "string")' /tmp/gema-smoke-package-list-python.json >/dev/null
  if "$HOSTD" package-verify-lock "$session" "$apt_lock" >/tmp/gema-smoke-package-drift.json 2>/tmp/gema-smoke-package-drift.err; then
    echo "ERROR: stale package lock did not report drift" >&2
    exit 1
  fi
  "$HOSTD" package-verify-lock "$session" latest | jq -e '.ok == true and .schema == "gema.package_verify.v1"' >/dev/null
  chroot "$rootfs" /usr/bin/env -i GEMA_CUDA_LANE=cu121 VIRTUAL_ENV=/opt/gema/ml/cu121 PATH=/opt/gema/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    pip install --disable-pip-version-check --no-cache-dir --only-binary=:all: rich >/tmp/gema-smoke-package-extra-python.txt
  if "$HOSTD" package-verify-lock "$session" latest >/tmp/gema-smoke-package-extra-drift.json 2>/tmp/gema-smoke-package-extra-drift.err; then
    echo "ERROR: extra python package did not report drift" >&2
    exit 1
  fi
  "$HOSTD" package-replay-lock "$session" latest | jq -e '.ok == true and .schema == "gema.package_verify.v1"' >/dev/null
  if chroot "$rootfs" /usr/bin/env -i GEMA_CUDA_LANE=cu121 VIRTUAL_ENV=/opt/gema/ml/cu121 PATH=/opt/gema/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    python -c 'import rich' >/tmp/gema-smoke-package-rich-import.txt 2>&1; then
    echo "ERROR: package replay did not remove extra python package" >&2
    exit 1
  fi
  "$HOSTD" mount-storage "$session" 67108864 >/tmp/gema-smoke-package-mount.json
  "$HOSTD" snapshot-create "$session" snap-pkg >/tmp/gema-smoke-package-snapshot.json
  jq -e '.ok == true and .snapshot.details.runtime_diff.included == true' /tmp/gema-smoke-package-snapshot.json >/dev/null
  cleanup_session "$session"
  assert_no_cgroup "$session"
  assert_no_bpf_pin "$session"
  assert_no_runtime_rootfs_mount "$session"
}

smoke_security_adversarial() {
  local session="smoke-security-adversarial"
  cleanup_session "$session"
  "$HOSTD" launch "$session" 20207 8127 tcp --health-path /status.txt --startup-timeout 120 --service-timeout 120 -- \
    /bin/bash -lc 'cd /home/gema/workspace
      : > status.txt
      if unshare -m true >/tmp/unshare.out 2>/tmp/unshare.err; then echo unshare-failed >> status.txt; else echo unshare-ok >> status.txt; fi
      mkdir -p /tmp/gema-mount-test
      if mount -t tmpfs tmpfs /tmp/gema-mount-test >/tmp/mount.out 2>/tmp/mount.err; then echo mount-failed >> status.txt; else echo mount-ok >> status.txt; fi
      python3 - <<"PY" >/tmp/ptrace.out 2>/tmp/ptrace.err
import ctypes, sys
libc = ctypes.CDLL(None, use_errno=True)
rc = libc.ptrace(0, 0, None, None)
sys.exit(0 if rc != 0 else 1)
PY
      if [ "$?" = "0" ]; then echo ptrace-ok >> status.txt; else echo ptrace-failed >> status.txt; fi
      if command -v bpftool >/dev/null 2>&1; then
        if bpftool prog list >/tmp/bpf.out 2>/tmp/bpf.err; then echo bpf-failed >> status.txt; else echo bpf-ok >> status.txt; fi
      else
        echo bpf-unavailable-ok >> status.txt
      fi
      exec python3 -m http.server 8127 --bind 0.0.0.0' >/tmp/gema-smoke-security-adversarial.json
  curl -fsS http://127.0.0.1:20207/status.txt >/tmp/gema-smoke-security-adversarial-status.txt
  grep -qx unshare-ok /tmp/gema-smoke-security-adversarial-status.txt
  grep -qx mount-ok /tmp/gema-smoke-security-adversarial-status.txt
  grep -qx ptrace-ok /tmp/gema-smoke-security-adversarial-status.txt
  grep -Eqx 'bpf-ok|bpf-unavailable-ok' /tmp/gema-smoke-security-adversarial-status.txt
  cleanup_session "$session"
  assert_no_port 20207
  assert_no_cgroup "$session"
  assert_no_bpf_pin "$session"
  assert_no_runtime_rootfs_mount "$session"
}

smoke_ml_recovery_contract() {
  local session="smoke-ml-recovery-contract"
  local agent
  cleanup_session "$session"
  "$HOSTD" launch "$session" 20209 8129 tcp --profile noble-ml-cu121 --health-path /status.txt --startup-timeout 120 --service-timeout 120 -- \
    /bin/bash -lc 'cd /home/gema/workspace && test -f .gema/recovery/resume.json && test -f .gema/recovery/framework-checkpoint-policy.json && python - <<"PY"
import json
resume = json.load(open(".gema/recovery/resume.json"))
policy = json.load(open(".gema/recovery/framework-checkpoint-policy.json"))
assert resume["schema"] == "gema.ml_resume.v1"
assert resume["mandatory"] is True
assert policy["framework_checkpoint"]["required_before_expensive_training"] is True
PY
printf ml-recovery-ok > status.txt
exec python3 -m http.server 8129 --bind 0.0.0.0' >/tmp/gema-smoke-ml-recovery-contract.json
  curl -fsS http://127.0.0.1:20209/status.txt | grep -qx ml-recovery-ok
  jq -e '.ml_recovery_contract.resume.schema == "gema.ml_resume.v1"' "$ROOT/sessions/$session/hostd.json" >/dev/null
  jq -e '.ml_bridge.enabled == true and .command[0] != "/opt/gema/bin/gema-ml-bridge" and .ml_bridge.command[0] == "/opt/gema/bin/gema-ml-bridge"' "$ROOT/sessions/$session/hostd.json" >/dev/null
  agent="$ROOT/workspaces/$session/agent/ml-bridge"
  jq -e '.schema == "gema.ml_bridge_state.v1" and .status == "running"' "$agent/bridge-state.json" >/dev/null
  jq -e '.schema == "gema.ml_bridge_heartbeat.v1" and .status == "running"' "$agent/bridge-heartbeat.json" >/dev/null
  cleanup_session "$session"
  assert_no_port 20209
  assert_no_cgroup "$session"
  assert_no_bpf_pin "$session"
  assert_no_runtime_rootfs_mount "$session"
  assert_no_session_network "$session"
}

smoke_execution_fabric_worker() {
  local session="smoke-execution-fabric"
  local workspace="$ROOT/workspaces/$session/workspace"
  local agent="$ROOT/workspaces/$session/agent"
  cleanup_session "$session"
  mkdir -p "$workspace/.gema/recovery" "$workspace/.gema/checkpoints" "$agent"
  "$HOSTD" execution-contract "$session" '{"command":["python3","-c","from pathlib import Path; Path(\".gema/checkpoints/worker.txt\").write_text(\"ok\")"],"cuda_lane":"cu121","require_gpu":false}' >/tmp/gema-smoke-fabric-contract.json
  cp "$workspace/.gema/fabric/execution_contract.json" /tmp/gema-smoke-fabric-contract-mirror.json
  cat > "$workspace/.gema/recovery/resume.json" <<EOF
{"schema":"gema.ml_resume.v1","session_id":"$session","mandatory":true}
EOF
  cat > "$workspace/.gema/recovery/framework-checkpoint-policy.json" <<'EOF'
{"schema":"gema.persistence_policy.v1","framework_checkpoint":{"required_before_expensive_training":true}}
EOF
  /root/gema-system/scripts/gema-accelerator-worker --workspace "$workspace" --agent-dir "$agent" --session-id "$session" --heartbeat-interval 0.1 >/tmp/gema-smoke-fabric-worker.out 2>/tmp/gema-smoke-fabric-worker.err
  test -f "$workspace/.gema/checkpoints/worker.txt"
  grep -qx ok "$workspace/.gema/checkpoints/worker.txt"
  jq -e '.schema == "gema.accelerator_state.v1" and .status == "completed" and .contract_checksum != null' "$workspace/.gema/fabric/accelerator_state.json" >/dev/null
  jq -e '.schema == "gema.accelerator_state.v1" and .status == "completed"' "$agent/fabric/accelerator_state.json" >/dev/null
  jq -e 'select(.type == "accelerator_worker_finished" and .details.status == "completed")' "$workspace/.gema/fabric/accelerator_worker.events.jsonl" >/dev/null
  cleanup_session "$session"
}

r2_remote_root() {
  local remote="${GEMA_SMOKE_R2_REMOTE:-${GEMA_R2_REMOTE:-${R2_REMOTE:-}}}"
  if [ -z "$remote" ] && [ -n "${R2_BUCKET:-}" ]; then
    remote="r2:${R2_BUCKET}"
  fi
  if [ -z "$remote" ] && [ -f /tmp/gema-r2-bucket ]; then
    remote="r2:$(cat /tmp/gema-r2-bucket)"
  fi
  if [ -z "$remote" ]; then
    remote="r2:gema-cloud-volumes"
  fi
  remote="${remote%/}"
  if [[ "$remote" != *:* ]] || [[ "$remote" == *: ]] || [[ "$remote" == *:/ ]]; then
    echo "ERROR: R2 smoke requires bucket-qualified remote, for example r2:<bucket>" >&2
    exit 78
  fi
  printf '%s\n' "$remote"
}

smoke_r2_snapshot_roundtrip() {
  command -v rclone >/dev/null || {
    echo "ERROR: rclone missing" >&2
    exit 78
  }
  local session="smoke-r2-$(date +%s)-$$"
  local remote_root prefix remote workspace snapdir sessiondir marker before_sha after_sha remote_list good_snapshot corrupt_snapshot partial_snapshot
  remote_root="$(r2_remote_root)"
  prefix="${GEMA_SMOKE_R2_PREFIX:-runtime-smoke}/${session}"
  remote="${remote_root}/${prefix}"
  workspace="$ROOT/workspaces/$session"
  snapdir="$workspace/snapshots"
  sessiondir="$ROOT/sessions/$session"
  marker="$workspace/workspace/r2-marker.txt"
  cleanup_session "$session"
  trap 'rclone purge "$remote" >/dev/null 2>&1 || true; "$HOSTD" unmount-storage "$session" delete >/dev/null 2>&1 || true; rm -rf "$sessiondir" "$workspace" >/dev/null 2>&1 || true' RETURN

  "$HOSTD" mount-storage "$session" 67108864 | jq -e '.ok == true' >/dev/null
  mkdir -p "$(dirname "$marker")"
  printf 'gema-r2-snapshot-smoke:%s\n' "$session" >"$marker"
  before_sha="$(sha256sum "$marker" | awk '{print $1}')"

  "$HOSTD" snapshot-create "$session" snap-a | jq -e '.ok == true and .snapshot.id == "snap-a"' >/dev/null
  test -f "$snapdir/snap-a.tar"
  test -f "$sessiondir/snapshots.json"

  "$HOSTD" r2-configure "$session" "$remote_root" "$prefix" | jq -e '.ok == true and (.r2.snapshot_remote | startswith("r2:"))' >/dev/null
  "$HOSTD" r2-status "$session" probe | jq -e '.ok == true and .probe.ok == true' >/dev/null
  "$HOSTD" r2-sync "$session" snapshots push | jq -e '.ok == true and .sync.ok == true' >/dev/null

  remote_list="$(rclone lsf "$remote/snapshots" --max-depth 1 | sort | tr '\n' ' ')"
  echo "$remote_list" | grep -q 'snap-a.tar'
  echo "$remote_list" | grep -q 'snapshots.json'
  echo "$remote_list" | grep -q 'recovery_plan.json'

  if [ "${GEMA_SMOKE_R2_FAILURES:-0}" = "1" ]; then
    good_snapshot="/tmp/${session}-snap-a.tar"
    corrupt_snapshot="/tmp/${session}-corrupt-snap-a.tar"
    partial_snapshot="/tmp/${session}-partial-snap-a.tar"
    cp "$snapdir/snap-a.tar" "$good_snapshot"
    printf 'corrupt-r2-object:%s\n' "$session" >"$corrupt_snapshot"
    rclone copyto "$corrupt_snapshot" "$remote/snapshots/snap-a.tar"
    rm -rf "$snapdir"
    rm -f "$sessiondir/snapshots.json" "$sessiondir/recovery_plan.json"
    "$HOSTD" r2-sync "$session" snapshots pull | jq -e '.ok == true and .sync.ok == true' >/dev/null
    if "$HOSTD" snapshot-restore "$session" snap-a >/tmp/gema-smoke-r2-corrupt-restore.json 2>/tmp/gema-smoke-r2-corrupt-restore.err; then
      echo "ERROR: corrupt R2 snapshot restored successfully" >&2
      exit 1
    fi
    grep -q 'checksum mismatch' /tmp/gema-smoke-r2-corrupt-restore.json

    head -c 1024 "$good_snapshot" >"$partial_snapshot"
    rclone copyto "$partial_snapshot" "$remote/snapshots/snap-a.tar"
    rm -rf "$snapdir"
    rm -f "$sessiondir/snapshots.json" "$sessiondir/recovery_plan.json"
    "$HOSTD" r2-sync "$session" snapshots pull | jq -e '.ok == true and .sync.ok == true' >/dev/null
    if "$HOSTD" snapshot-restore "$session" snap-a >/tmp/gema-smoke-r2-partial-restore.json 2>/tmp/gema-smoke-r2-partial-restore.err; then
      echo "ERROR: partial R2 snapshot restored successfully" >&2
      exit 1
    fi
    grep -q 'checksum mismatch' /tmp/gema-smoke-r2-partial-restore.json

    rclone deletefile "$remote/snapshots/snap-a.tar"
    rm -rf "$snapdir"
    rm -f "$sessiondir/snapshots.json" "$sessiondir/recovery_plan.json"
    "$HOSTD" r2-sync "$session" snapshots pull | jq -e '.ok == true and .sync.ok == true' >/dev/null
    if "$HOSTD" snapshot-restore "$session" snap-a >/tmp/gema-smoke-r2-missing-restore.json 2>/tmp/gema-smoke-r2-missing-restore.err; then
      echo "ERROR: missing R2 snapshot restored successfully" >&2
      exit 1
    fi
    grep -Eq 'missing|No such file|not found' /tmp/gema-smoke-r2-missing-restore.json

    rclone copyto "$good_snapshot" "$remote/snapshots/snap-a.tar"
    rm -f "$good_snapshot" "$corrupt_snapshot" "$partial_snapshot"
  fi

  printf 'gema-r2-snapshot-smoke-updated:%s\n' "$session" >"$marker"
  "$HOSTD" snapshot-create "$session" snap-b | jq -e '.ok == true and .snapshot.id == "snap-b"' >/dev/null
  "$HOSTD" r2-sync "$session" snapshots push | jq -e '.ok == true and .sync.ok == true' >/dev/null
  remote_list="$(rclone lsf "$remote/snapshots" --max-depth 1 | sort | tr '\n' ' ')"
  echo "$remote_list" | grep -q 'snap-a.tar'
  echo "$remote_list" | grep -q 'snap-b.tar'
  rclone cat "$remote/snapshots/snapshots.json" | jq -e '([.snapshots[] | select(.id == "snap-a")] | length) == 1 and ([.snapshots[] | select(.id == "snap-b")] | length) == 1' >/dev/null

  rm -rf "$snapdir"
  rm -f "$sessiondir/snapshots.json" "$sessiondir/recovery_plan.json"
  "$HOSTD" r2-sync "$session" snapshots pull | jq -e '.ok == true and .sync.ok == true' >/dev/null
  test -f "$snapdir/snap-a.tar"
  test -f "$sessiondir/snapshots.json"

  rm -f "$marker"
  "$HOSTD" snapshot-restore "$session" snap-a | jq -e '.ok == true and .restore.snapshot.id == "snap-a"' >/dev/null
  after_sha="$(sha256sum "$marker" | awk '{print $1}')"
  test "$before_sha" = "$after_sha"

  "$HOSTD" r2-sync "$session" workspace push | jq -e '.ok == true and .sync.ok == true' >/dev/null
  rclone lsf "$remote/workspace" --max-depth 1 | grep -q 'r2-marker.txt'
}

smoke_persistence_policy() {
  command -v rclone >/dev/null || {
    echo "ERROR: rclone missing" >&2
    exit 78
  }
  local session="smoke-persistence-policy"
  local remote_root=":local:/tmp/gema-persistence-policy"
  local prefix="$session"
  local marker="$ROOT/workspaces/$session/workspace/persistence-marker.txt"
  cleanup_session "$session"
  trap 'rclone purge ":local:/tmp/gema-persistence-policy" >/dev/null 2>&1 || true; "$HOSTD" unmount-storage "$session" delete >/dev/null 2>&1 || true; rm -rf "$ROOT/sessions/$session" "$ROOT/workspaces/$session" >/dev/null 2>&1 || true' RETURN

  "$HOSTD" mount-storage "$session" 67108864 | jq -e '.ok == true' >/dev/null
  mkdir -p "$(dirname "$marker")"
  printf 'persistent-state\n' >"$marker"
  "$HOSTD" r2-configure "$session" "$remote_root" "$prefix" | jq -e '.ok == true' >/dev/null
  "$HOSTD" snapshot-create "$session" snap-policy | jq -e '.ok == true and .snapshot.id == "snap-policy"' >/dev/null
  "$HOSTD" r2-sync "$session" snapshots push | jq -e '.ok == true and .sync.ok == true' >/dev/null
  "$HOSTD" persistence-policy "$session" managed 60 | jq -e '.ok == true and .persistence_policy.schema == "gema.persistence_policy.v1" and .persistence_status.schema == "gema.persistence_status.v1"' >/dev/null
  "$HOSTD" persistence-verify "$session" | jq -e '.ok == true and .persistence.verification.ok == true and .persistence.policy.last_verified_restore_point.ok == true' >/dev/null
  "$HOSTD" recovery-plan "$session" | jq -e '.ok == true and .recovery_plan.persistence.schema == "gema.persistence_status.v1"' >/dev/null
}

require_root
cd "$SYSTEM_DIR"

section "build and unit tests"
cargo test
cargo build

section "hostd restart"
if command -v systemctl >/dev/null 2>&1 && systemctl list-unit-files gema-hostd.service >/dev/null 2>&1; then
  "$HOSTD" stop-daemon >/dev/null 2>&1 || true
  systemctl restart gema-hostd.service
else
  "$HOSTD" stop-daemon >/dev/null 2>&1 || true
  "$HOSTD" start >/dev/null
fi
sleep 1
"$HOSTD" status | jq -e '.ok == true' >/dev/null

section "host probe"
"$HOSTD" host-probe free | jq -e '
  .ok == true and
  ([.checks[] | select(.required == true and .status != "pass")] | length) == 0
' >/dev/null

section "session id rejection"
if "$HOSTD" inspect '../escape' >/tmp/gema-invalid-id.out 2>&1; then
  echo "ERROR: invalid session id was accepted" >&2
  exit 1
fi

section "http launch, eBPF attach, cleanup"
cleanup_session smoke-http
"$HOSTD" launch smoke-http 20201 8121 tcp --health-path /status.txt --startup-timeout 120 --service-timeout 120 -- \
  /bin/bash -lc 'cd /home/gema/workspace && printf fresh-ok > status.txt && exec python3 -m http.server 8121 --bind 0.0.0.0' >/tmp/gema-smoke-http.json
curl -fsS http://127.0.0.1:20201/status.txt | grep -qx fresh-ok
"$HOSTD" inspect smoke-http | jq -e '.lifecycle.state == "running" and .security.device_policy == "cgroup-v2-ebpf"' >/dev/null
bpftool cgroup show /sys/fs/cgroup/gema-system/smoke-http | grep -q gema_device_policy
cleanup_session smoke-http
assert_no_port 20201
assert_no_cgroup smoke-http
assert_no_bpf_pin smoke-http
assert_no_runtime_rootfs_mount smoke-http
assert_no_session_network smoke-http

section "device denial"
rm -f "$SMOKE_ROOTFS/tmp/gema-deny-kmsg"
mknod "$SMOKE_ROOTFS/tmp/gema-deny-kmsg" c 1 11
chmod 666 "$SMOKE_ROOTFS/tmp/gema-deny-kmsg"
cleanup_session smoke-bpf-deny
"$HOSTD" launch smoke-bpf-deny 20202 8122 tcp --health-path /status.txt --startup-timeout 120 --service-timeout 120 -- \
  /bin/bash -lc 'cd /home/gema/workspace; cat /dev/null >/tmp/allow-result 2>&1 && echo allow-ok > status.txt; if timeout 2 head -c1 /tmp/gema-deny-kmsg >/tmp/deny-out 2>/tmp/deny-err; then echo deny-failed >> status.txt; else echo deny-ok >> status.txt; fi; exec python3 -m http.server 8122 --bind 0.0.0.0' >/tmp/gema-smoke-bpf-deny.json
curl -fsS http://127.0.0.1:20202/status.txt | grep -q deny-ok
cleanup_session smoke-bpf-deny
rm -f "$SMOKE_ROOTFS/tmp/gema-deny-kmsg"
assert_no_port 20202
assert_no_cgroup smoke-bpf-deny
assert_no_bpf_pin smoke-bpf-deny
assert_no_runtime_rootfs_mount smoke-bpf-deny
assert_no_session_network smoke-bpf-deny

section "adversarial security syscalls"
smoke_security_adversarial

section "ML recovery contract"
smoke_ml_recovery_contract

section "execution fabric worker"
smoke_execution_fabric_worker

section "restart recovery"
cleanup_session smoke-restart
"$HOSTD" launch smoke-restart 20203 8123 tcp --health-path /status.txt --startup-timeout 120 --service-timeout 120 --restart-policy on-failure --restart-max 2 -- \
  /bin/bash -lc 'cd /home/gema/workspace && if [ ! -f .once ]; then touch .once; exit 42; fi; printf restarted > status.txt; exec python3 -m http.server 8123 --bind 0.0.0.0' >/tmp/gema-smoke-restart.json 2>/tmp/gema-smoke-restart.err || true
for _ in $(seq 1 90); do
  if curl -fsS http://127.0.0.1:20203/status.txt 2>/dev/null | grep -qx restarted; then
    break
  fi
  sleep 1
done
curl -fsS http://127.0.0.1:20203/status.txt | grep -qx restarted
"$HOSTD" inspect smoke-restart | jq -e '.lifecycle.state == "running"' >/dev/null
cleanup_session smoke-restart
assert_no_port 20203
assert_no_cgroup smoke-restart
assert_no_bpf_pin smoke-restart
assert_no_runtime_rootfs_mount smoke-restart
assert_no_session_network smoke-restart

section "udp routing"
cleanup_session smoke-udp
"$HOSTD" launch smoke-udp 20204 8124 udp --startup-timeout 120 --service-timeout 120 -- \
  /bin/bash -lc 'python3 - <<"PY"
import socket
s=socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(("0.0.0.0",8124))
while True:
    data, addr = s.recvfrom(4096)
    s.sendto(b"udp-ok", addr)
PY' >/tmp/gema-smoke-udp.json
python3 - <<'PY'
import socket
s=socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.settimeout(5)
s.sendto(b"ping", ("127.0.0.1", 20204))
data, _ = s.recvfrom(4096)
assert data == b"udp-ok", data
PY
cleanup_session smoke-udp
assert_no_cgroup smoke-udp
assert_no_bpf_pin smoke-udp
assert_no_runtime_rootfs_mount smoke-udp
assert_no_session_network smoke-udp

section "failed launch network cleanup"
smoke_failed_launch_network_cleanup

section "concurrent rootfs isolation"
smoke_concurrent_rootfs_isolation

section "snapshot session state and runtime diff"
smoke_snapshot_runtime_layers

section "persistence policy and verified restore point"
smoke_persistence_policy

section "process checkpoint and restore"
smoke_process_checkpoint

if [ "${GEMA_SMOKE_CRIU_EXTENDED:-0}" = "1" ]; then
  section "process checkpoint extended matrix"
  smoke_process_checkpoint_extended
fi

if [ "${GEMA_SMOKE_PACKAGES:-0}" = "1" ]; then
  section "package installer"
  smoke_package_installer
fi

section "image promotion and non-launchable profile rejection"
"$HOSTD" image-pull docker.io/library/busybox:latest | jq -e '.ok == true and .rootfs.ok == true' >/dev/null
test -f "$ROOT/rootfs/docker_io_library_busybox_latest/.gema-rootfs-ready"
if "$HOSTD" launch smoke-busybox 20205 8125 tcp --profile docker_io_library_busybox_latest -- /bin/sh -c 'sleep 1' >/tmp/gema-smoke-busybox.out 2>&1; then
  echo "ERROR: non-launchable busybox profile was accepted" >&2
  cleanup_session smoke-busybox
  exit 1
fi
"$HOSTD" ports | jq -e '.allocations == {}' >/dev/null
assert_no_port 20205
assert_no_cgroup smoke-busybox
assert_no_runtime_rootfs_mount smoke-busybox

if [ "${GEMA_SMOKE_R2:-0}" = "1" ]; then
  section "r2 snapshot durability"
  smoke_r2_snapshot_roundtrip
fi

section "done"
"$HOSTD" ports
