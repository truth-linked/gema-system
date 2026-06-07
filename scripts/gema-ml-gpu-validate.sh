#!/usr/bin/env bash
set -euo pipefail

ROOT="${GEMA_SYSTEM_ROOT:-/var/lib/gema-system}"
HOSTD="${GEMA_HOSTD:-/root/gema-system/scripts/gema-hostd.sh}"
GPU_DEVICE="${GEMA_GPU_DEVICE_ID:-0}"
LANES="${GEMA_GPU_VALIDATE_LANES:-cu121 cu124}"
PORT_BASE="${GEMA_GPU_VALIDATE_PORT_BASE:-22120}"

if [ "$(id -u)" != "0" ]; then
  echo "ERROR: run as root on a GPU worker" >&2
  exit 77
fi
if ! command -v nvidia-smi >/dev/null 2>&1; then
  echo "ERROR: nvidia-smi missing" >&2
  exit 78
fi

nvidia-smi >/tmp/gema-gpu-validate-nvidia-smi.txt

lane_profile() {
  case "$1" in
    cu118|cu121|cu124) printf 'noble-ml-%s\n' "$1" ;;
    *) echo "ERROR: unsupported lane: $1" >&2; exit 64 ;;
  esac
}

lane_python_smoke() {
  local lane="$1"
  cat <<PY
import json, os, pathlib, subprocess, sys
status = {"lane": "$lane", "checks": {}}
def check(name, fn):
    try:
        status["checks"][name] = fn()
    except Exception as exc:
        status["checks"][name] = {"ok": False, "error": repr(exc)}

def torch_cuda():
    import torch
    ok = torch.cuda.is_available()
    result = {
        "ok": bool(ok),
        "torch": torch.__version__,
        "torch_cuda": torch.version.cuda,
        "device_count": torch.cuda.device_count(),
    }
    if ok:
        x = torch.ones((128, 128), device="cuda")
        result["sum"] = float((x @ x).sum().item())
        result["device_name"] = torch.cuda.get_device_name(0)
    return result

def import_package(name):
    __import__(name)
    return {"ok": True}

check("torch_cuda", torch_cuda)
check("bitsandbytes_import", lambda: import_package("bitsandbytes"))
check("xformers_import", lambda: import_package("xformers"))
check("deepspeed_import", lambda: import_package("deepspeed"))
if "$lane" != "cu118":
    check("vllm_import", lambda: import_package("vllm"))

def nccl_single_process():
    import torch
    import torch.distributed as dist
    if not torch.cuda.is_available():
        return {"ok": False, "error": "cuda unavailable"}
    os.environ.setdefault("MASTER_ADDR", "127.0.0.1")
    os.environ.setdefault("MASTER_PORT", "29591")
    dist.init_process_group("nccl", rank=0, world_size=1)
    value = torch.ones(1, device="cuda")
    dist.all_reduce(value)
    dist.destroy_process_group()
    return {"ok": float(value.item()) == 1.0}
check("nccl_single_process", nccl_single_process)

pathlib.Path(".gema/gpu-validation.json").write_text(json.dumps(status, indent=2, sort_keys=True) + "\\n")
failed = [name for name, result in status["checks"].items() if not result.get("ok")]
if failed:
    print(json.dumps(status, indent=2, sort_keys=True))
    sys.exit(2)
print(json.dumps(status, indent=2, sort_keys=True))
PY
}

idx=0
for lane in $LANES; do
  profile="$(lane_profile "$lane")"
  session="gpu-validate-$lane-$(date +%s)-$$"
  port=$((PORT_BASE + idx))
  idx=$((idx + 1))
  echo "== validating $profile on GPU $GPU_DEVICE =="
  "$HOSTD" cleanup "$session" >/dev/null 2>&1 || true
  "$HOSTD" assign-gpu "$session" nvidia "$GPU_DEVICE" exclusive >/tmp/gema-gpu-validate-assign-"$lane".json
  "$HOSTD" launch "$session" "$port" 8180 tcp --profile "$profile" --health-path /status.txt --startup-timeout 180 --service-timeout 180 -- \
    /bin/bash -lc "$(printf 'cd /home/gema/workspace && python - <<'"'"'PY'"'"'\n%s\nPY\nprintf ok > status.txt\nexec python3 -m http.server 8180 --bind 0.0.0.0' "$(lane_python_smoke "$lane")")" \
    >/tmp/gema-gpu-validate-launch-"$lane".json
  curl -fsS "http://127.0.0.1:$port/status.txt" | grep -qx ok
  agent="$ROOT/workspaces/$session/agent/ml-bridge"
  jq -e '.schema == "gema.ml_bridge_state.v1"' "$agent/bridge-state.json" >/dev/null
  workspace="$(jq -r '.storage_volume.mountpoint' "$ROOT/sessions/$session/hostd.json")"
  echo "-- $lane validation --"
  cat "$workspace/.gema/gpu-validation.json"
  "$HOSTD" cleanup "$session" >/dev/null
done

echo "GPU validation passed for lanes: $LANES"
