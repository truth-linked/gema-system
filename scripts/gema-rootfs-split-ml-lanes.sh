#!/usr/bin/env bash
set -euo pipefail

ROOT="${GEMA_SYSTEM_ROOT:-/var/lib/gema-system}"
SOURCE="${GEMA_SOURCE_ROOTFS:-$ROOT/rootfs/noble-base}"
LANES="${GEMA_ML_LANES:-cu118 cu121 cu124}"
SUITE="${GEMA_SUITE:-noble}"
ARCH="${GEMA_ARCH:-amd64}"
DELETE_SOURCE="${GEMA_DELETE_SOURCE:-0}"

if [ "$(id -u)" != "0" ]; then
  echo "ERROR: run as root" >&2
  exit 77
fi

if [ ! -d "$SOURCE" ] || [ ! -f "$SOURCE/.gema-rootfs-ready" ]; then
  echo "ERROR: source rootfs is not ready: $SOURCE" >&2
  exit 66
fi

for required in usr/bin/env usr/bin/setpriv bin/bash opt/gema/bin/gema-cuda-select opt/gema/bin/gema-fabric-bridge opt/gema/bin/gema-ml-bridge opt/gema/bin/gema-accelerator-worker opt/gema/bin/gema-cluster-bridge opt/gema/bin/gema-cuda-proxy-daemon opt/gema/cuda-proxy/lib/libcuda.so.1 opt/gema/bin/gvphase1-backend opt/gema/gvphase1/lib/libcuda.so.1 opt/gema/ml-requirements.txt opt/gema/cuda-lanes.json; do
  if [ ! -e "$SOURCE/$required" ]; then
    echo "ERROR: source rootfs missing $required" >&2
    exit 66
  fi
done

lane_cuda() {
  case "$1" in
    cu118) printf '11.8\n' ;;
    cu121) printf '12.1\n' ;;
    cu124) printf '12.4\n' ;;
    *) echo "ERROR: unsupported ML lane: $1" >&2; exit 66 ;;
  esac
}

write_release() {
  local rootfs="$1" lane="$2"
  rm -f "$rootfs/etc/gema-release"
  cat > "$rootfs/etc/gema-release" <<EOF
NAME="Gema Linux ML Wheel Substrate"
BASE="Ubuntu $SUITE"
GEMA_NATIVE="1"
GEMA_ML_REQUIREMENTS="1"
GEMA_CUDA_LANES="1"
GEMA_WHEEL_ONLY="1"
GEMA_ML_LANE="$lane"
EOF
}

write_manifest() {
  local rootfs="$1" image_id="$2" lane="$3" cuda="$4"
  python3 - "$rootfs" "$SUITE" "$ARCH" "$image_id" "$lane" "$cuda" <<'PY'
import hashlib
import json
import os
import sys
import time

rootfs, suite, arch, image_id, lane, cuda = sys.argv[1:7]

def read(path):
    try:
        with open(os.path.join(rootfs, path), "rb") as handle:
            return handle.read()
    except FileNotFoundError:
        return b""

def kv(path):
    values = {}
    for line in read(path).decode("utf-8", errors="replace").splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            values[key] = value.strip().strip('"')
    return values

def command_exists(name):
    for prefix in ("opt/gema/bin", "usr/local/sbin", "usr/local/bin", "usr/sbin", "usr/bin", "sbin", "bin"):
        if os.path.exists(os.path.join(rootfs, prefix, name)):
            return True
    return False

def package_count():
    return sum(1 for line in read("var/lib/dpkg/status").decode("utf-8", errors="replace").splitlines() if line.startswith("Package: "))

digest = hashlib.sha256()
for relative in ("etc/os-release", "etc/gema-release", ".gema-rootfs-ready", ".gema-ml-ready", "opt/gema/ml-requirements.txt", "opt/gema/cuda-lanes.json", "var/lib/dpkg/status", f"opt/gema/ml/{lane}/gema.freeze.txt"):
    digest.update(relative.encode())
    digest.update(read(relative))

os_release = kv("etc/os-release")
ready = read(".gema-rootfs-ready").decode("utf-8", errors="replace").strip() or time.strftime("%Y-%m-%dT%H:%M:%S%z")
lane_python = f"/opt/gema/ml/{lane}/bin/python"
manifest = {
    "id": image_id,
    "name": f"Gema Linux ML {lane}",
    "profile": "gpu-ml",
    "version": ready,
    "checksum": f"sha256:{digest.hexdigest()}",
    "os": {
        "id": os_release.get("ID"),
        "name": os_release.get("PRETTY_NAME"),
        "version_id": os_release.get("VERSION_ID"),
        "codename": os_release.get("VERSION_CODENAME"),
        "architecture": arch,
    },
    "capabilities": {
        "runtime": ["gema-native"],
        "os": ["linux"],
        "commands": {name: command_exists(name) for name in ("bash", "python3", "curl", "ip", "sudo", "ssh", "sshd", "ssh-keygen", "ffmpeg", "git", "gema-cuda-select", "gema-fabric-bridge", "gema-ml-bridge", "gema-accelerator-worker", "gema-cluster-bridge", "gema-cuda-proxy-daemon", "gvphase1-backend")},
        "desktop": {"xfce": command_exists("xfce4-session"), "pipewire": command_exists("pipewire"), "sunshine": command_exists("sunshine")},
        "gpu": {"nvidia_userland": False, "cuda_toolkits": [], "cuda_path": None, "driver_required_from_host": True},
        "ml": {
            "python3": command_exists("python3"),
            "selector": command_exists("gema-cuda-select"),
            "fabric_bridge": command_exists("gema-fabric-bridge"),
            "bridge": command_exists("gema-ml-bridge"),
            "accelerator_worker": command_exists("gema-accelerator-worker"),
            "cluster_bridge": command_exists("gema-cluster-bridge"),
            "cuda_proxy": os.path.exists(os.path.join(rootfs, "opt/gema/gvphase1/lib/libcuda.so.1")),
            "cuda_proxy_kind": "gvphase1_binary_cuda_api_remoting" if os.path.exists(os.path.join(rootfs, "opt/gema/gvphase1/lib/libcuda.so.1")) else "legacy_driver_rpc_prototype",
            "managed_env_root": "/opt/gema/ml",
            "wheel_only": True,
            "default_lane": lane,
            "lanes": {lane: {"python": lane_python, "freeze": os.path.exists(os.path.join(rootfs, lane_python.lstrip("/")).replace("/bin/python", "/gema.freeze.txt")), "cuda": cuda}},
            "requirements": os.path.exists(os.path.join(rootfs, "opt/gema/ml-requirements.txt")),
            "jupyter": os.path.exists(os.path.join(rootfs, f"opt/gema/ml/{lane}/bin/jupyter")),
        },
    },
    "compatibility": {
        "runtime": ["gema-native"],
        "gpu_vendors": ["cpu", "nvidia"],
        "requires": {"min_nvidia_driver": None, "cuda": {"python_lanes": [lane], "toolkits": []}, "kernel_features": ["cgroup-v2", "user-namespace", "network-namespace"]},
    },
    "packages": {"count": package_count()},
    "generated_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
}
path = os.path.join(rootfs, "etc/gema-image.json")
tmp = f"{path}.tmp"
with open(tmp, "w", encoding="utf-8") as handle:
    json.dump(manifest, handle, indent=2, sort_keys=True)
    handle.write("\n")
os.replace(tmp, path)
PY
}

split_lane() {
  local lane="$1" image_id dest next old
  local cuda
  image_id="noble-ml-$lane"
  dest="$ROOT/rootfs/$image_id"
  next="$dest.next"
  cuda="$(lane_cuda "$lane")"
  if [ ! -x "$SOURCE/opt/gema/ml/$lane/bin/python" ]; then
    echo "ERROR: source rootfs missing Python lane: $lane" >&2
    exit 66
  fi

  echo "[gema-rootfs-split] creating $image_id from $SOURCE"
  rm -rf "$next"
  cp -al "$SOURCE" "$next"

  rm -rf "$next/usr/local/cuda" "$next"/usr/local/cuda-* "$next/opt/gema/cuda"
  rm -f "$next/etc/apt/sources.list.d/cuda-"*.list "$next/etc/apt/sources.list.d/cuda"*.list
  find "$next/opt/gema/ml" -mindepth 1 -maxdepth 1 -type d ! -name "$lane" -exec rm -rf {} +
  find "$next/opt/gema/ml" -mindepth 1 -maxdepth 1 -type f -delete

  rm -f "$next/.gema-ml-ready"
  printf 'gema.ml.wheel-only.%s.%s\n' "$lane" "$(sha256sum "$next/opt/gema/ml/$lane/gema.freeze.txt" | awk '{print $1}')" > "$next/.gema-ml-ready"
  write_release "$next" "$lane"
  write_manifest "$next" "$image_id" "$lane" "$cuda"

  chroot "$next" /opt/gema/ml/"$lane"/bin/python -m pip check >/tmp/gema-"$image_id"-pip-check.out
  chroot "$next" /usr/bin/env -i GEMA_CUDA_LANE="$lane" PATH=/opt/gema/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin /opt/gema/bin/python - <<'PY'
import torch
print(torch.__version__, torch.version.cuda)
PY

  old="$dest.old.$(date +%s)"
  if [ -e "$dest" ]; then
    mv "$dest" "$old"
  fi
  mv "$next" "$dest"
  rm -rf "$old"
  echo "[gema-rootfs-split] ready: $dest"
}

for lane in $LANES; do
  split_lane "$lane"
done

if [ "$DELETE_SOURCE" = "1" ]; then
  case "$SOURCE" in
    "$ROOT/rootfs/noble-base")
      echo "[gema-rootfs-split] deleting oversized source: $SOURCE"
      rm -rf "$SOURCE"
      ;;
    *)
      echo "ERROR: refusing to delete non-default source: $SOURCE" >&2
      exit 66
      ;;
  esac
fi
