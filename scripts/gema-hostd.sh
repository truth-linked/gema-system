#!/usr/bin/env bash
set -euo pipefail

ROOT="${GEMA_SYSTEM_ROOT:-/var/lib/gema-system}"
RUN_DIR="${GEMA_HOSTD_RUN_DIR:-/run/gema-system}"
LOG_DIR="${GEMA_HOSTD_LOG_DIR:-$ROOT/logs}"
PID_FILE="$RUN_DIR/hostd.pid"
SUPERVISOR_PID_FILE="$RUN_DIR/hostd.supervisor.pid"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSTEM_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
if [ -n "${GEMA_HOSTD_BIN:-}" ]; then
  HOSTD_BIN="$GEMA_HOSTD_BIN"
elif [ -x "$SYSTEM_DIR/target/release/gema-runtime" ]; then
  HOSTD_BIN="$SYSTEM_DIR/target/release/gema-runtime"
else
  HOSTD_BIN="$SYSTEM_DIR/target/debug/gema-runtime"
fi
export GEMA_SYSTEM_SCRIPT_DIR="$SCRIPT_DIR"

usage() {
  cat >&2 <<'EOF'
usage:
  gema-hostd.sh foreground
  gema-hostd.sh start
  gema-hostd.sh stop-daemon
  gema-hostd.sh status
  gema-hostd.sh inspect <session_id>
  gema-hostd.sh logs <session_id> [lines|-f]
  gema-hostd.sh metrics <session_id>
  gema-hostd.sh storage <session_id>
  gema-hostd.sh set-storage-quota <session_id> <quota_bytes> [snapshot_retention_count]
  gema-hostd.sh cleanup-storage <session_id>
  gema-hostd.sh sweep-workspace-mounts [--dry-run] [--delete-empty]
  gema-hostd.sh mount-storage <session_id> [quota_bytes]
  gema-hostd.sh unmount-storage <session_id> [delete]
  gema-hostd.sh launch-readiness <session_id> <host_port|auto> <guest_port> [tcp|udp] [--profile <id>] -- <command...>
  gema-hostd.sh host-probe [free|paid-gpu] [gpu_vendor] [require_r2]
  gema-hostd.sh ports
  gema-hostd.sh allocate-port <session_id> <guest_port> <public|private> [host_port|auto] [tcp|udp]
  gema-hostd.sh release-ports <session_id>
  gema-hostd.sh processes <session_id>
  gema-hostd.sh package-install <session_id> <apt|python|pip|npm|node> <packages...>
  gema-hostd.sh package-list <session_id>
  gema-hostd.sh package-verify-lock <session_id> [latest|lock.json]
  gema-hostd.sh package-replay-lock <session_id> [latest|lock.json]
  gema-hostd.sh images
  gema-hostd.sh image-compat <profile|any> [gpu_vendor] [runtime] [cuda]
  gema-hostd.sh event <session_id> <event_type> [json-details] [actor] [severity]
  gema-hostd.sh snapshot <session_id> <started|completed|failed> [json-details]
  gema-hostd.sh snapshot-create <session_id> [snapshot_id]
  gema-hostd.sh snapshot-restore <session_id> [snapshot_id]
  gema-hostd.sh process-checkpoint-create <session_id> [checkpoint_id] [pid]
  gema-hostd.sh process-checkpoint-restore <session_id> [checkpoint_id] [terminate-existing|allow-live]
  gema-hostd.sh process-checkpoints <session_id>
  gema-hostd.sh recovery-plan <session_id>
  gema-hostd.sh execution-contract <session_id> [json-details]
  gema-hostd.sh accelerator-attach <session_id> [json-details]
  gema-hostd.sh accelerator-detach <session_id> [drain|force] [json-details]
  gema-hostd.sh accelerator-status <session_id>
  gema-hostd.sh fabric-contract <session_id> [json-details]
  gema-hostd.sh fabric-status <session_id>
  gema-hostd.sh cuda-proxy-contract <session_id> [json-details]
  gema-hostd.sh cuda-proxy-status <session_id>
  gema-hostd.sh browser-stream-start <session_id>
  gema-hostd.sh browser-stream-status <session_id>
  gema-hostd.sh browser-stream-stop <session_id>
  gema-hostd.sh browser-stream-revoke <session_id>
  gema-hostd.sh r2-configure <session_id> [remote] [prefix]
  gema-hostd.sh r2-status <session_id> [probe]
  gema-hostd.sh r2-sync <session_id> [workspace|snapshots] [push|pull]
  gema-hostd.sh r2-mount <session_id> [workspace|snapshots]
  gema-hostd.sh r2-unmount <session_id> [workspace|snapshots]
  gema-hostd.sh user-s3-status <session_id> [probe]
  gema-hostd.sh user-s3-sync <session_id> [workspace|snapshots] [push|pull]
  gema-hostd.sh user-s3-mount <session_id> [workspace|snapshots]
  gema-hostd.sh user-s3-unmount <session_id> [workspace|snapshots]
  gema-hostd.sh checkpoint-policy <session_id> [managed|user-s3] [interval_seconds]
  gema-hostd.sh persistence-policy <session_id> [managed|user-s3] [interval_seconds]
  gema-hostd.sh persistence-status <session_id>
  gema-hostd.sh persistence-verify <session_id>
  gema-hostd.sh agent-socket <session_id>
  gema-hostd.sh agent-api <session_id> [action] [json-details]
  gema-hostd.sh assign-gpu <session_id> [nvidia|amd] [device_id] [mode]
  gema-hostd.sh gpu-status <session_id>
  gema-hostd.sh gpu-attached <session_id> [json-details]
  gema-hostd.sh agent-action <session_id> <action> [json-details]
  gema-hostd.sh user-command <session_id> <command...>
  gema-hostd.sh terminal-command <session_id> <command...>
  gema-hostd.sh image-verify <reference> [profile]
  gema-hostd.sh image-pull <reference>
  gema-hostd.sh image-prune
  gema-hostd.sh launch <session_id> <host_port|auto> <guest_port> [tcp|udp] [--health-path <path>] [--startup-timeout <seconds>] [--service-timeout <seconds>] [--health-timeout-ms <ms>] [--restart-policy <never|on-failure|always>] [--restart-max <count>] [--profile <id>] -- <command...>
  gema-hostd.sh stop <session_id>
  gema-hostd.sh cleanup <session_id>
EOF
}

require_root() {
  if [ "$(id -u)" != "0" ]; then
    echo "ERROR: run as root" >&2
    exit 77
  fi
}

setup() {
  mkdir -p "$RUN_DIR" "$LOG_DIR"
  if [ ! -x "$HOSTD_BIN" ]; then
    (cd "$SYSTEM_DIR" && cargo build >/dev/null)
  fi
}

is_running() {
  [ -s "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" >/dev/null 2>&1
}

ensure_running() {
  setup
  if is_running && [ -S "$RUN_DIR/hostd.sock" ]; then
    return 0
  fi
  if command -v systemctl >/dev/null 2>&1 && systemctl is-active --quiet gema-hostd.service; then
    for _ in $(seq 1 50); do
      if [ -S "$RUN_DIR/hostd.sock" ]; then
        return 0
      fi
      sleep 0.1
    done
  fi
  rm -f "$RUN_DIR/hostd.sock" "$PID_FILE"
  if [ ! -s "$SUPERVISOR_PID_FILE" ] || ! kill -0 "$(cat "$SUPERVISOR_PID_FILE")" >/dev/null 2>&1; then
    setsid "$0" foreground </dev/null >> "$LOG_DIR/hostd-supervisor.log" 2>&1 &
    echo "$!" > "$SUPERVISOR_PID_FILE"
  fi
  for _ in $(seq 1 50); do
    if is_running && [ -S "$RUN_DIR/hostd.sock" ]; then
      return 0
    fi
    sleep 0.1
  done
  echo "ERROR: gema-hostd did not start" >&2
  exit 70
}

case "${1:-}" in
  foreground)
    require_root
    setup
    echo "$$" > "$SUPERVISOR_PID_FILE"
    trap 'rm -f "$SUPERVISOR_PID_FILE"' EXIT
    while true; do
      set +e
      "$HOSTD_BIN" hostd serve
      status=$?
      set -e
      echo "gema-hostd child exited status=$status; restarting in 1s" >&2
      sleep 1
    done
    ;;
  start)
    require_root
    setup
    if [ -s "$SUPERVISOR_PID_FILE" ] && kill -0 "$(cat "$SUPERVISOR_PID_FILE")" >/dev/null 2>&1; then
      echo "gema-hostd already running supervisor=$(cat "$SUPERVISOR_PID_FILE")"
      exit 0
    fi
    setsid "$0" foreground </dev/null >> "$LOG_DIR/hostd-supervisor.log" 2>&1 &
    echo "started gema-hostd pid=$!"
    ;;
  stop-daemon)
    require_root
    if [ -S "$RUN_DIR/hostd.sock" ]; then
      "$HOSTD_BIN" hostd shutdown >/dev/null 2>&1 || true
    fi
    if [ -s "$PID_FILE" ]; then
      kill "$(cat "$PID_FILE")" >/dev/null 2>&1 || true
      rm -f "$PID_FILE"
    fi
    if [ -s "$SUPERVISOR_PID_FILE" ]; then
      kill "$(cat "$SUPERVISOR_PID_FILE")" >/dev/null 2>&1 || true
      rm -f "$SUPERVISOR_PID_FILE"
    fi
    ;;
  status)
    ensure_running
    if [ -S "$RUN_DIR/hostd.sock" ] && [ -s "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" >/dev/null 2>&1; then
      "$HOSTD_BIN" hostd status
    else
      echo '{"ok":false,"status":"inactive"}'
      exit 3
    fi
    ;;
  launch)
    [ "$#" -ge 6 ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  launch-readiness)
    [ "$#" -ge 6 ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  inspect)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  logs)
    [ -n "${2:-}" ] || { usage; exit 64; }
    if [ "${3:-}" = "-f" ]; then
      exec tail -F "$ROOT/sessions/$2"/logs/*.log
    fi
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  metrics)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  storage)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  set-storage-quota)
    [ "$#" -ge 3 ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  cleanup-storage)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  sweep-workspace-mounts)
    "$SCRIPT_DIR/gema-workspace-mount-sweep.sh" "${@:2}"
    ;;
  mount-storage)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  unmount-storage)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  host-probe)
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  ports)
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  allocate-port)
    [ "$#" -ge 4 ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  release-ports)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  processes)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  package-install)
    [ "$#" -ge 4 ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  package-list)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  package-verify-lock|package-replay-lock)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  images)
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  image-compat)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  image-verify|image-pull)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  image-prune)
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  event)
    [ "$#" -ge 3 ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  snapshot)
    [ "$#" -ge 3 ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  snapshot-create|snapshot-restore|process-checkpoint-create|process-checkpoint-restore|process-checkpoints|recovery-plan|execution-contract|accelerator-attach|accelerator-detach|accelerator-status|fabric-contract|fabric-status|cuda-proxy-contract|cuda-proxy-status|browser-stream-start|browser-stream-status|browser-stream-stop|browser-stream-revoke|r2-configure|r2-status|r2-sync|r2-mount|r2-unmount|user-s3-status|user-s3-sync|user-s3-mount|user-s3-unmount|checkpoint-policy|persistence-policy|persistence-status|persistence-verify|agent-socket|agent-api|assign-gpu|gpu-status)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  gpu-attached)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    details="${3:-}"
    [ -n "$details" ] || details="{}"
    "$HOSTD_BIN" hostd event "$2" gpu_attached "$details" system info
    ;;
  agent-action)
    [ "$#" -ge 3 ] || { usage; exit 64; }
    ensure_running
    details="${4:-}"
    [ -n "$details" ] || details="{}"
    payload="$(python3 -c 'import json,sys
action=sys.argv[1]
try:
    details=json.loads(sys.argv[2])
except json.JSONDecodeError:
    details={"raw": sys.argv[2]}
print(json.dumps({"action": action, "details": details}))' "$3" "$details")"
    "$HOSTD_BIN" hostd event "$2" agent_action "$payload" agent info
    ;;
  user-command)
    [ "$#" -ge 3 ] || { usage; exit 64; }
    ensure_running
    session_id="$2"
    shift 2
    command_json="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "$@")"
    "$HOSTD_BIN" hostd event "$session_id" user_command "{\"command\":$command_json}" user info
    ;;
  terminal-command)
    [ "$#" -ge 3 ] || { usage; exit 64; }
    ensure_running
    session_id="$2"
    shift 2
    command_json="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "$@")"
    "$HOSTD_BIN" hostd event "$session_id" terminal_command "{\"command\":$command_json}" user info
    ;;
  stop)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  cleanup)
    [ -n "${2:-}" ] || { usage; exit 64; }
    ensure_running
    "$HOSTD_BIN" hostd "$@"
    ;;
  *)
    usage
    exit 64
    ;;
esac
