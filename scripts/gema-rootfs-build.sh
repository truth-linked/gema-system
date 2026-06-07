#!/usr/bin/env bash
set -euo pipefail

ROOT="${GEMA_SYSTEM_ROOT:-/var/lib/gema-system}"
SUITE="${GEMA_SUITE:-noble}"
ARCH="${GEMA_ARCH:-amd64}"
MIRROR="${GEMA_MIRROR:-http://archive.ubuntu.com/ubuntu}"
ROOTFS="${GEMA_ROOTFS:-$ROOT/rootfs/$SUITE-base}"
IMAGE_ID="${GEMA_IMAGE_ID:-$(basename "$ROOTFS")}"
IMAGE_PROFILE="${GEMA_IMAGE_PROFILE:-base}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSTEM_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
GEMA_CLOUD_DIR="${GEMA_CLOUD_DIR:-/root/gema_cloud}"
ML_REQUIREMENTS="${GEMA_ML_REQUIREMENTS:-$SYSTEM_DIR/rootfs/ml-requirements.txt}"
ML_EXPANSION_REQUIREMENTS="${GEMA_ML_EXPANSION_REQUIREMENTS:-$SYSTEM_DIR/rootfs/ml-expansion-requirements.txt}"
ML_RECOVERY_POLICIES="${GEMA_ML_RECOVERY_POLICIES:-$SYSTEM_DIR/rootfs/ml-recovery-policies.json}"
CUDA_LANES_FILE="${GEMA_CUDA_LANES_FILE:-$SYSTEM_DIR/rootfs/cuda-lanes.json}"
CUDA_SELECT_SCRIPT="${GEMA_CUDA_SELECT_SCRIPT:-$SYSTEM_DIR/scripts/gema-cuda-select.sh}"
FABRIC_BRIDGE_SCRIPT="${GEMA_FABRIC_BRIDGE_SCRIPT:-$SYSTEM_DIR/scripts/gema-fabric-bridge}"
CUDA_PROXY_DIR="${GEMA_CUDA_PROXY_DIR:-$SYSTEM_DIR/rootfs/cuda-proxy}"
GVPHASE1_DIR="${GEMA_GVPHASE1_DIR:-/root/gvphase1}"
ML_ROOT="/opt/gema/ml"
DEFAULT_ML_LANES="${GEMA_ML_LANES:-cu118 cu121 cu124}"
DEFAULT_CUDA_LANE="${GEMA_DEFAULT_CUDA_LANE:-cu121}"
INSTALL_CUDA_TOOLKITS="${GEMA_INSTALL_CUDA_TOOLKITS:-0}"
STRICT_LANE_POLICY="${GEMA_STRICT_LANE_POLICY:-1}"
METADATA_ONLY="${GEMA_METADATA_ONLY:-0}"
case "$IMAGE_PROFILE" in
  gpu-ml|ml) DEFAULT_INSTALL_ML_STACK=1 ;;
  *) DEFAULT_INSTALL_ML_STACK=0 ;;
esac
INSTALL_ML_STACK="${GEMA_INSTALL_ML_STACK:-$DEFAULT_INSTALL_ML_STACK}"
INSTALL_MANAGED_WORKSPACE="${GEMA_INSTALL_MANAGED_WORKSPACE:-1}"
LANE_POLICY_VERSION="gema-lane-policy-v2"
WHEEL_ONLY_PRUNE_VERSION="gema-wheel-only-prune-v2"
CUDA_TOOLKIT_PACKAGES="${GEMA_CUDA_TOOLKIT_PACKAGES:-cuda-toolkit-12-1 cuda-toolkit-12-2 cuda-toolkit-12-3 cuda-toolkit-12-4 cuda-toolkit-12-5 cuda-toolkit-12-6 cuda-toolkit-12-8 cuda-toolkit-12-9 cuda-toolkit-13-0 cuda-toolkit-13-1 cuda-toolkit-13-2}"
CUDA_OPTIONAL_PACKAGES="${GEMA_CUDA_OPTIONAL_PACKAGES:-libcudnn9-cuda-12 libcudnn9-dev-cuda-12 libnccl2 libnccl-dev}"
BUILD_MOUNTS=()

cleanup_build_mounts() {
  local idx path
  for ((idx=${#BUILD_MOUNTS[@]}-1; idx>=0; idx--)); do
    path="${BUILD_MOUNTS[$idx]}"
    if mountpoint -q "$path"; then
      umount "$path" || umount -l "$path" || true
    fi
  done
}
trap cleanup_build_mounts EXIT

if [ "$(id -u)" != "0" ]; then
  echo "ERROR: run as root" >&2
  exit 77
fi

if ! command -v debootstrap >/dev/null 2>&1; then
  echo "[gema-rootfs] installing debootstrap"
  apt-get update
  apt-get install -y debootstrap ca-certificates
fi

configure_apt_sources() {
  cat > "$ROOTFS/etc/apt/sources.list" <<EOF
deb $MIRROR $SUITE main restricted universe multiverse
deb $MIRROR $SUITE-updates main restricted universe multiverse
deb http://security.ubuntu.com/ubuntu $SUITE-security main restricted universe multiverse
EOF
}

install_cuda_repo() {
  if [ "$INSTALL_CUDA_TOOLKITS" != "1" ]; then
    return 0
  fi
  mkdir -p "$ROOTFS/opt/gema/cuda"
  if [ -f "$ROOTFS/etc/apt/sources.list.d/cuda-ubuntu2404-x86_64.list" ]; then
    return 0
  fi
  echo "[gema-rootfs] enabling NVIDIA CUDA repository for Ubuntu 24.04"
  chroot "$ROOTFS" env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates curl gnupg
  chroot "$ROOTFS" /bin/bash -lc '
    set -euo pipefail
    tmp=/tmp/cuda-keyring_1.1-1_all.deb
    curl -fsSL https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2404/x86_64/cuda-keyring_1.1-1_all.deb -o "$tmp"
    dpkg -i "$tmp"
    rm -f "$tmp"
  '
}

install_available_apt_packages() {
  local manifest="$1"
  shift
  mkdir -p "$(dirname "$manifest")"
  : > "$manifest"
  local pkg
  for pkg in "$@"; do
    if chroot "$ROOTFS" apt-cache show "$pkg" >/dev/null 2>&1; then
      echo "[gema-rootfs] installing $pkg"
      chroot "$ROOTFS" env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "$pkg"
      printf 'installed %s\n' "$pkg" >> "$manifest"
    else
      echo "[gema-rootfs] skipping unavailable CUDA package: $pkg"
      printf 'unavailable %s\n' "$pkg" >> "$manifest"
    fi
  done
}

install_cuda_toolkits() {
  if [ "$INSTALL_CUDA_TOOLKITS" != "1" ]; then
    echo "[gema-rootfs] CUDA toolkit install disabled"
    return 0
  fi
  configure_apt_sources
  mount_for_chroot
  chroot "$ROOTFS" env DEBIAN_FRONTEND=noninteractive apt-get update
  install_cuda_repo
  chroot "$ROOTFS" env DEBIAN_FRONTEND=noninteractive apt-get update
  # Install only versioned packages. Do not install driver metas inside the rootfs.
  install_available_apt_packages "$ROOTFS/opt/gema/cuda/toolkit-packages.txt" $CUDA_TOOLKIT_PACKAGES
  install_available_apt_packages "$ROOTFS/opt/gema/cuda/optional-packages.txt" $CUDA_OPTIONAL_PACKAGES
  chroot "$ROOTFS" /bin/bash -lc '
    set -euo pipefail
    mkdir -p /opt/gema/cuda
    {
      echo "gema.cuda.toolkits.v1"
      for path in /usr/local/cuda-*; do
        [ -d "$path" ] || continue
        version="${path##*/cuda-}"
        echo "$version $path"
      done
    } > /opt/gema/cuda/installed-toolkits.txt
  '
}

copy_if_present() {
  local src="$1" dst="$2" mode="${3:-0755}"
  if [ -f "$src" ]; then
    install -m "$mode" "$src" "$dst"
  fi
}

install_cuda_proxy() {
  if [ ! -f "$CUDA_PROXY_DIR/gema_cuda_proxy_client.c" ] ||
    [ ! -f "$CUDA_PROXY_DIR/gema-cuda-proxy-daemon.py" ] ||
    [ ! -x "$CUDA_PROXY_DIR/build.sh" ]; then
    echo "ERROR: CUDA proxy source missing under $CUDA_PROXY_DIR" >&2
    exit 66
  fi
  if ! command -v gcc >/dev/null 2>&1; then
    echo "ERROR: gcc is required to build Gema CUDA proxy client" >&2
    exit 66
  fi
  local build_dir
  build_dir="$(mktemp -d /tmp/gema-cuda-proxy.XXXXXX)"
  "$CUDA_PROXY_DIR/build.sh" "$build_dir" >/dev/null
  mkdir -p "$ROOTFS/opt/gema/cuda-proxy/lib" "$ROOTFS/opt/gema/cuda-proxy/bin" "$ROOTFS/opt/gema/bin"
  install -m 0755 "$build_dir/lib/libcuda.so.1" "$ROOTFS/opt/gema/cuda-proxy/lib/libcuda.so.1"
  ln -sf libcuda.so.1 "$ROOTFS/opt/gema/cuda-proxy/lib/libcuda.so"
  install -m 0755 "$build_dir/bin/gema-cuda-proxy-daemon" "$ROOTFS/opt/gema/cuda-proxy/bin/gema-cuda-proxy-daemon"
  ln -sf ../cuda-proxy/bin/gema-cuda-proxy-daemon "$ROOTFS/opt/gema/bin/gema-cuda-proxy-daemon"
  cat > "$ROOTFS/opt/gema/bin/gema-cuda-proxy-env" <<'EOF'
#!/bin/sh
endpoint="${GEMA_CUDA_PROXY_ENDPOINT:-127.0.0.1:47619}"
printf 'GEMA_CUDA_PROXY_ENDPOINT=%s\n' "$endpoint"
printf 'LD_LIBRARY_PATH=/opt/gema/cuda-proxy/lib:${LD_LIBRARY_PATH:-}\n'
EOF
  chmod 0755 "$ROOTFS/opt/gema/bin/gema-cuda-proxy-env"
  rm -rf "$build_dir"
}

install_gvphase1_cuda_proxy() {
  if [ "${GEMA_INSTALL_GVPHASE1:-1}" != "1" ]; then
    echo "[gema-rootfs] GVPhase1 CUDA proxy install disabled"
    return 0
  fi
  if [ ! -f "$GVPHASE1_DIR/Makefile" ]; then
    echo "[gema-rootfs] GVPhase1 source not found at $GVPHASE1_DIR; keeping legacy CUDA proxy only"
    return 0
  fi
  echo "[gema-rootfs] installing GVPhase1 CUDA API remoting data plane"
  make -C "$GVPHASE1_DIR" all >/dev/null
  mkdir -p "$ROOTFS/opt/gema/gvphase1/bin" "$ROOTFS/opt/gema/gvphase1/lib" "$ROOTFS/opt/gema/gvphase1/include" "$ROOTFS/opt/gema/bin"
  install -m 0755 "$GVPHASE1_DIR/build/bin/gvphase1-backend" "$ROOTFS/opt/gema/gvphase1/bin/gvphase1-backend"
  install -m 0755 "$GVPHASE1_DIR"/build/lib/*.so* "$ROOTFS/opt/gema/gvphase1/lib/"
  install -m 0644 "$GVPHASE1_DIR"/include/*.h "$ROOTFS/opt/gema/gvphase1/include/"
  ln -sf ../gvphase1/bin/gvphase1-backend "$ROOTFS/opt/gema/bin/gvphase1-backend"
  cat > "$ROOTFS/opt/gema/bin/gema-gvphase1-env" <<'EOF'
#!/bin/sh
endpoint="${GVPHASE1_ENDPOINT:-${GEMA_CUDA_PROXY_ENDPOINT:-127.0.0.1:47619}}"
printf 'GVPHASE1_ENDPOINT=%s\n' "$endpoint"
printf 'GEMA_CUDA_PROXY_ENDPOINT=%s\n' "$endpoint"
printf 'LD_LIBRARY_PATH=/opt/gema/gvphase1/lib:${LD_LIBRARY_PATH:-}\n'
EOF
  chmod 0755 "$ROOTFS/opt/gema/bin/gema-gvphase1-env"
  cat > "$ROOTFS/opt/gema/gvphase1/contract.json" <<'EOF'
{
  "schema": "gema.gvphase1.contract.v1",
  "role": "primary_cuda_api_remoting_data_plane",
  "frontend": "Gema workspace LD_LIBRARY_PATH /opt/gema/gvphase1/lib",
  "backend": "gvphase1-backend on attached GPU host",
  "control_plane": "gema-fabric-bridge only for provisioning, tunnels, logs, checkpoints, and lifecycle",
  "not_a_task_offload_bridge": true
}
EOF
}

install_managed_workspace_stack() {
  if [ "$INSTALL_MANAGED_WORKSPACE" != "1" ]; then
    return 0
  fi
  echo "[gema-rootfs] installing managed CPU workspace stack"
  configure_apt_sources
  mount_for_chroot
  chroot "$ROOTFS" env DEBIAN_FRONTEND=noninteractive apt-get update
  chroot "$ROOTFS" env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    ca-certificates curl bash coreutils findutils procps iproute2 iputils-ping less vim-tiny \
    util-linux python3 python3-venv python3-pip python3-gi openssh-client openssh-server fuse3 rclone \
    jq wget rsync tmux htop nano git nodejs npm ffmpeg

  mkdir -p "$ROOTFS/usr/local/bin" "$ROOTFS/etc/profile.d" "$ROOTFS/opt/gema-agent" \
    "$ROOTFS/home/gema/workspace" "$ROOTFS/home/gema/.gema/agent/logs" "$ROOTFS/opt/gema/bin"

  copy_if_present "$GEMA_CLOUD_DIR/docker/snapshot-daemon.sh" "$ROOTFS/usr/local/bin/snapshot-daemon"
  copy_if_present "$GEMA_CLOUD_DIR/docker/snapshot-restore.sh" "$ROOTFS/usr/local/bin/snapshot-restore"
  copy_if_present "$GEMA_CLOUD_DIR/docker/app-checkpoint.sh" "$ROOTFS/usr/local/bin/gema-app-checkpoint"
  copy_if_present "$GEMA_CLOUD_DIR/docker/recovery-orchestrator.sh" "$ROOTFS/usr/local/bin/gema-recovery-orchestrator"
  copy_if_present "$GEMA_CLOUD_DIR/docker/gema-run" "$ROOTFS/usr/local/bin/gema-run"
  copy_if_present "$GEMA_CLOUD_DIR/docker/gema-pipeline" "$ROOTFS/usr/local/bin/gema-pipeline"
  copy_if_present "$GEMA_CLOUD_DIR/docker/gpu-runtime-report" "$ROOTFS/usr/local/bin/gema-gpu-runtime-report"
  copy_if_present "$GEMA_CLOUD_DIR/docker/gema-real-guard" "$ROOTFS/usr/local/bin/gema-real-guard"
  copy_if_present "$GEMA_CLOUD_DIR/docker/gema-command-audit" "$ROOTFS/usr/local/bin/gema-command-audit"
  copy_if_present "$GEMA_CLOUD_DIR/docker/gema-audit-profile.sh" "$ROOTFS/etc/profile.d/gema-audit.sh" 0644
  mkdir -p "$ROOTFS/opt/gema/ml/auto" "$ROOTFS/opt/gema/bin"
  if [ ! -x "$ROOTFS/opt/gema/ml/auto/bin/python" ]; then
    chroot "$ROOTFS" python3 -m venv /opt/gema/ml/auto
    chroot "$ROOTFS" /opt/gema/ml/auto/bin/python -m ensurepip --upgrade
    chroot "$ROOTFS" /opt/gema/ml/auto/bin/python -m pip install --disable-pip-version-check --no-cache-dir --upgrade pip setuptools wheel
  fi
  chroot "$ROOTFS" chown -R 1000:1000 /opt/gema/ml/auto
  cat > "$ROOTFS/opt/gema/bin/python" <<'EOF'
#!/bin/sh
workspace="${GEMA_PROJECT_DIR:-/home/gema/workspace}"
workspace_required="${GEMA_REQUIRE_WORKSPACE_STORAGE:-0}"
workspace_fstype="$(findmnt -rn -M "$workspace" -o FSTYPE 2>/dev/null || true)"
if [ "$workspace_required" = "1" ] && [ "$workspace_fstype" != "fuse.rclone" ] && [ "$workspace_fstype" != "rclone" ]; then
  echo "ERROR: workspace storage is not mounted through R2; refusing local user writes" >&2
  exit 70
fi
if [ -d "$workspace" ] && mkdir -p "$workspace/.gema/tmp" "$workspace/.gema/cache/pip" "$workspace/.gema/cache/xdg" "$workspace/.gema/env" 2>/dev/null; then
  default_env="$workspace/.gema/env/python-auto"
  site_packages="$workspace/.gema/python/site-packages"
  user_base="$workspace/.gema/python/userbase"
  mkdir -p "$site_packages" "$user_base"
  export TMPDIR="${TMPDIR:-$workspace/.gema/tmp}"
  export PIP_CACHE_DIR="${PIP_CACHE_DIR:-$workspace/.gema/cache/pip}"
  export XDG_CACHE_HOME="${XDG_CACHE_HOME:-$workspace/.gema/cache/xdg}"
  export PYTHONUSERBASE="${PYTHONUSERBASE:-$user_base}"
  export PYTHONPATH="${PYTHONPATH:+$PYTHONPATH:}$site_packages"
  export PIP_TARGET="${PIP_TARGET:-$site_packages}"
else
  if [ "$workspace_required" = "1" ]; then
    echo "ERROR: workspace storage is not mounted or writable; refusing local user writes" >&2
    exit 70
  fi
  default_env="/opt/gema/ml/auto"
fi
if [ "$workspace_required" = "1" ]; then
  unset VIRTUAL_ENV
  export PATH="/opt/gema/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
  exec /usr/bin/python3 "$@"
fi
env_dir="${GEMA_PYTHON_ENV:-${VIRTUAL_ENV:-$default_env}}"
if [ "$env_dir" = "/opt/gema/ml/auto" ] && [ -d "$workspace" ] && [ "${GEMA_ALLOW_LOCAL_ENV:-0}" != "1" ]; then
  env_dir="$default_env"
fi
if [ ! -x "$env_dir/bin/python" ]; then
  mkdir -p "$(dirname "$env_dir")"
  /usr/bin/python3 -m venv --copies "$env_dir"
  "$env_dir/bin/python" -m ensurepip --upgrade >/dev/null 2>&1 || true
fi
if [ ! -x "$env_dir/bin/python" ]; then
  echo "ERROR: python environment could not be created on workspace storage" >&2
  exit 70
fi
export VIRTUAL_ENV="$env_dir"
export PATH="/opt/gema/bin:$env_dir/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
exec "$env_dir/bin/python" "$@"
EOF
  cat > "$ROOTFS/opt/gema/bin/python3" <<'EOF'
#!/bin/sh
exec /opt/gema/bin/python "$@"
EOF
  cat > "$ROOTFS/opt/gema/bin/pip" <<'EOF'
#!/bin/sh
workspace="${GEMA_PROJECT_DIR:-/home/gema/workspace}"
workspace_required="${GEMA_REQUIRE_WORKSPACE_STORAGE:-0}"
workspace_fstype="$(findmnt -rn -M "$workspace" -o FSTYPE 2>/dev/null || true)"
if [ "$workspace_required" = "1" ] && [ "$workspace_fstype" != "fuse.rclone" ] && [ "$workspace_fstype" != "rclone" ]; then
  echo "ERROR: workspace storage is not mounted through R2; refusing local user writes" >&2
  exit 70
fi
if [ -d "$workspace" ] && mkdir -p "$workspace/.gema/tmp" "$workspace/.gema/cache/pip" "$workspace/.gema/cache/xdg" "$workspace/.gema/env" 2>/dev/null; then
  default_env="$workspace/.gema/env/python-auto"
  site_packages="$workspace/.gema/python/site-packages"
  user_base="$workspace/.gema/python/userbase"
  mkdir -p "$site_packages" "$user_base"
  export TMPDIR="${TMPDIR:-$workspace/.gema/tmp}"
  export PIP_CACHE_DIR="${PIP_CACHE_DIR:-$workspace/.gema/cache/pip}"
  export XDG_CACHE_HOME="${XDG_CACHE_HOME:-$workspace/.gema/cache/xdg}"
  export PYTHONUSERBASE="${PYTHONUSERBASE:-$user_base}"
  export PYTHONPATH="${PYTHONPATH:+$PYTHONPATH:}$site_packages"
  export PIP_TARGET="${PIP_TARGET:-$site_packages}"
else
  if [ "$workspace_required" = "1" ]; then
    echo "ERROR: workspace storage is not mounted or writable; refusing local user writes" >&2
    exit 70
  fi
  default_env="/opt/gema/ml/auto"
fi
if [ "$workspace_required" = "1" ]; then
  unset VIRTUAL_ENV
  export PATH="/opt/gema/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
  exec /usr/bin/python3 -m pip "$@"
fi
env_dir="${GEMA_PYTHON_ENV:-${VIRTUAL_ENV:-$default_env}}"
if [ "$env_dir" = "/opt/gema/ml/auto" ] && [ -d "$workspace" ] && [ "${GEMA_ALLOW_LOCAL_ENV:-0}" != "1" ]; then
  env_dir="$default_env"
fi
if [ ! -x "$env_dir/bin/python" ]; then
  mkdir -p "$(dirname "$env_dir")"
  /usr/bin/python3 -m venv --copies "$env_dir"
  "$env_dir/bin/python" -m ensurepip --upgrade >/dev/null 2>&1 || true
fi
if [ ! -x "$env_dir/bin/python" ]; then
  echo "ERROR: python environment could not be created on workspace storage" >&2
  exit 70
fi
export VIRTUAL_ENV="$env_dir"
export PATH="/opt/gema/bin:$env_dir/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
exec "$env_dir/bin/python" -m pip "$@"
EOF
  cat > "$ROOTFS/opt/gema/bin/pip3" <<'EOF'
#!/bin/sh
exec /opt/gema/bin/pip "$@"
EOF
  cat > "$ROOTFS/opt/gema/bin/gema-apt-r2" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

workspace="${GEMA_WORKSPACE_ROOT:-/home/gema}/workspace"
prefix="${GEMA_APT_PREFIX:-$workspace/.gema/apt}"
remote="${GEMA_R2_WORKSPACE_REMOTE:-}"
session="${SESSION_ID:-${GEMA_SESSION_ID:-default}}"
work="${GEMA_APT_WORKDIR:-/tmp/gema-apt/$session}"

if [ "${GEMA_REQUIRE_WORKSPACE_STORAGE:-0}" = "1" ]; then
  if ! findmnt -rn -M "$workspace" -o FSTYPE 2>/dev/null | grep -Eq '^(fuse\.rclone|rclone)$'; then
    echo "ERROR: apt requires a healthy R2-backed workspace mount" >&2
    exit 70
  fi
fi

mkdir -p "$work/cache/archives/partial" "$work/lists/partial" "$work/state" "$work/root" "$work/log" "$prefix"
touch "$work/state/status"

copy_from_r2() {
  [ -n "$remote" ] || return 0
  command -v rclone >/dev/null 2>&1 || return 0
  rclone copy "$remote/.gema/apt/state" "$work/state" --links >/dev/null 2>&1 || true
  rclone copy "$remote/.gema/apt/lists" "$work/lists" --links >/dev/null 2>&1 || true
}

sync_to_r2() {
  if [ -n "$remote" ] && command -v rclone >/dev/null 2>&1; then
    rclone copy "$work/state" "$remote/.gema/apt/state" --links --checksum >/dev/null 2>&1 || true
    rclone copy "$work/lists" "$remote/.gema/apt/lists" --links --checksum >/dev/null 2>&1 || true
    rclone copy "$work/cache" "$remote/.gema/apt/cache" --links --checksum >/dev/null 2>&1 || true
    rclone copy "$work/root" "$remote/.gema/apt/root" --links --checksum >/dev/null 2>&1 || true
  else
    mkdir -p "$prefix"
    cp -a "$work/state" "$work/lists" "$work/cache" "$work/root" "$prefix/" 2>/dev/null || true
  fi
  if [ -d "$prefix/root" ]; then
    find "$prefix/root" -type d -exec chmod 0755 {} + 2>/dev/null || true
    for dir in \
      "$prefix/root/bin" "$prefix/root/sbin" \
      "$prefix/root/usr/bin" "$prefix/root/usr/sbin" \
      "$prefix/root/usr/local/bin" "$prefix/root/usr/local/sbin"; do
      [ -d "$dir" ] && find "$dir" -type f -exec chmod 0755 {} + 2>/dev/null || true
    done
  fi
}

apt_opts=(
  -o "Dir::State=$work/state"
  -o "Dir::State::status=$work/state/status"
  -o "Dir::State::lists=$work/lists"
  -o "Dir::Cache=$work/cache"
  -o "Dir::Cache::archives=$work/cache/archives"
  -o "Dir::Log=$work/log"
  -o "Debug::NoLocking=true"
)

cmd="${1:-}"
[ -n "$cmd" ] || { echo "usage: apt update|install|download ..." >&2; exit 64; }
shift || true
copy_from_r2

case "$cmd" in
  update)
    /usr/bin/apt-get "${apt_opts[@]}" update "$@"
    sync_to_r2
    ;;
  download)
    /usr/bin/apt-get "${apt_opts[@]}" download "$@"
    sync_to_r2
    ;;
  install)
    /usr/bin/apt-get "${apt_opts[@]}" --download-only install "$@"
    shopt -s nullglob
    debs=("$work/cache/archives/"*.deb)
    for deb in "${debs[@]}"; do
      dpkg-deb -x "$deb" "$work/root"
    done
    cat > "$work/activate.sh" <<ACTIVATE
export PATH="$prefix/root/usr/local/sbin:$prefix/root/usr/local/bin:$prefix/root/usr/sbin:$prefix/root/usr/bin:\$PATH"
export LD_LIBRARY_PATH="$prefix/root/usr/local/lib:$prefix/root/usr/lib:$prefix/root/usr/lib/x86_64-linux-gnu:$prefix/root/lib:$prefix/root/lib/x86_64-linux-gnu:\${LD_LIBRARY_PATH:-}"
export PKG_CONFIG_PATH="$prefix/root/usr/local/lib/pkgconfig:$prefix/root/usr/lib/pkgconfig:$prefix/root/usr/lib/x86_64-linux-gnu/pkgconfig:\${PKG_CONFIG_PATH:-}"
export CMAKE_PREFIX_PATH="$prefix/root/usr:$prefix/root/usr/local:\${CMAKE_PREFIX_PATH:-}"
export CPATH="$prefix/root/usr/include:$prefix/root/usr/include/x86_64-linux-gnu:\${CPATH:-}"
ACTIVATE
    sync_to_r2
    echo "Installed package payloads into $prefix/root"
    ;;
  *)
    echo "ERROR: apt command '$cmd' is not supported in R2 workspace mode." >&2
    exit 64
    ;;
esac
EOF
  cat > "$ROOTFS/opt/gema/bin/apt" <<'EOF'
#!/usr/bin/env bash
exec /opt/gema/bin/gema-apt-r2 "$@"
EOF
  cat > "$ROOTFS/opt/gema/bin/apt-get" <<'EOF'
#!/usr/bin/env bash
exec /opt/gema/bin/gema-apt-r2 "$@"
EOF
  chmod 0755 "$ROOTFS/opt/gema/bin/python" "$ROOTFS/opt/gema/bin/python3" "$ROOTFS/opt/gema/bin/pip" "$ROOTFS/opt/gema/bin/pip3" "$ROOTFS/opt/gema/bin/gema-apt-r2" "$ROOTFS/opt/gema/bin/apt" "$ROOTFS/opt/gema/bin/apt-get"
  cat > "$ROOTFS/etc/profile.d/gema-python-env.sh" <<'EOF'
if [ -d /home/gema/workspace ]; then
  export GEMA_PYTHON_ENV="${GEMA_PYTHON_ENV:-/home/gema/workspace/.gema/env/python-auto}"
  export VIRTUAL_ENV="${VIRTUAL_ENV:-$GEMA_PYTHON_ENV}"
else
  export VIRTUAL_ENV="${VIRTUAL_ENV:-/opt/gema/ml/auto}"
fi
case ":$PATH:" in
  *":/opt/gema/bin:"*) ;;
  *) export PATH="/opt/gema/bin:$VIRTUAL_ENV/bin:$PATH" ;;
esac
if [ -d /home/gema/workspace ]; then
  mkdir -p /home/gema/workspace/.gema/tmp /home/gema/workspace/.gema/cache/pip /home/gema/workspace/.gema/cache/xdg 2>/dev/null || true
  export TMPDIR="${TMPDIR:-/home/gema/workspace/.gema/tmp}"
  export PIP_CACHE_DIR="${PIP_CACHE_DIR:-/home/gema/workspace/.gema/cache/pip}"
  export XDG_CACHE_HOME="${XDG_CACHE_HOME:-/home/gema/workspace/.gema/cache/xdg}"
  export NPM_CONFIG_CACHE="${NPM_CONFIG_CACHE:-/home/gema/workspace/.gema/cache/npm}"
  export NPM_CONFIG_PREFIX="${NPM_CONFIG_PREFIX:-/home/gema/workspace/.gema/node}"
  export YARN_CACHE_FOLDER="${YARN_CACHE_FOLDER:-/home/gema/workspace/.gema/cache/yarn}"
  export CARGO_HOME="${CARGO_HOME:-/home/gema/workspace/.gema/home/.cargo}"
  export RUSTUP_HOME="${RUSTUP_HOME:-/home/gema/workspace/.gema/home/.rustup}"
  export HF_HOME="${HF_HOME:-/home/gema/workspace/.gema/cache/huggingface}"
  export TRANSFORMERS_CACHE="${TRANSFORMERS_CACHE:-/home/gema/workspace/.gema/cache/huggingface/transformers}"
  export TORCH_HOME="${TORCH_HOME:-/home/gema/workspace/.gema/cache/torch}"
  export GEMA_APT_PREFIX="${GEMA_APT_PREFIX:-/home/gema/workspace/.gema/apt}"
  case ":$PATH:" in
    *":$GEMA_APT_PREFIX/root/usr/bin:"*) ;;
    *) export PATH="$GEMA_APT_PREFIX/root/usr/local/sbin:$GEMA_APT_PREFIX/root/usr/local/bin:$GEMA_APT_PREFIX/root/usr/sbin:$GEMA_APT_PREFIX/root/usr/bin:$PATH" ;;
  esac
  export LD_LIBRARY_PATH="$GEMA_APT_PREFIX/root/usr/local/lib:$GEMA_APT_PREFIX/root/usr/lib:$GEMA_APT_PREFIX/root/usr/lib/x86_64-linux-gnu:$GEMA_APT_PREFIX/root/lib:$GEMA_APT_PREFIX/root/lib/x86_64-linux-gnu:${LD_LIBRARY_PATH:-}"
  export PKG_CONFIG_PATH="$GEMA_APT_PREFIX/root/usr/local/lib/pkgconfig:$GEMA_APT_PREFIX/root/usr/lib/pkgconfig:$GEMA_APT_PREFIX/root/usr/lib/x86_64-linux-gnu/pkgconfig:${PKG_CONFIG_PATH:-}"
  export CMAKE_PREFIX_PATH="$GEMA_APT_PREFIX/root/usr:$GEMA_APT_PREFIX/root/usr/local:${CMAKE_PREFIX_PATH:-}"
  export CPATH="$GEMA_APT_PREFIX/root/usr/include:$GEMA_APT_PREFIX/root/usr/include/x86_64-linux-gnu:${CPATH:-}"
fi
EOF
  cat > "$ROOTFS/usr/local/bin/gema-terminal" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
export HOME="${HOME:-/home/gema}"
export USER="${USER:-gema}"
export LOGNAME="${LOGNAME:-$USER}"
if [ -d /home/gema/workspace ]; then
  export GEMA_PYTHON_ENV="${GEMA_PYTHON_ENV:-/home/gema/workspace/.gema/env/python-auto}"
  export VIRTUAL_ENV="${VIRTUAL_ENV:-$GEMA_PYTHON_ENV}"
else
  export VIRTUAL_ENV="${VIRTUAL_ENV:-/opt/gema/ml/auto}"
fi
export PATH="/opt/gema/bin:$VIRTUAL_ENV/bin:$PATH"
if [ -d /home/gema/workspace ]; then
  mkdir -p /home/gema/workspace/.gema/tmp /home/gema/workspace/.gema/cache/pip /home/gema/workspace/.gema/cache/xdg 2>/dev/null || true
  export TMPDIR="${TMPDIR:-/home/gema/workspace/.gema/tmp}"
  export PIP_CACHE_DIR="${PIP_CACHE_DIR:-/home/gema/workspace/.gema/cache/pip}"
  export XDG_CACHE_HOME="${XDG_CACHE_HOME:-/home/gema/workspace/.gema/cache/xdg}"
  export NPM_CONFIG_CACHE="${NPM_CONFIG_CACHE:-/home/gema/workspace/.gema/cache/npm}"
  export NPM_CONFIG_PREFIX="${NPM_CONFIG_PREFIX:-/home/gema/workspace/.gema/node}"
  export YARN_CACHE_FOLDER="${YARN_CACHE_FOLDER:-/home/gema/workspace/.gema/cache/yarn}"
  export CARGO_HOME="${CARGO_HOME:-/home/gema/workspace/.gema/home/.cargo}"
  export RUSTUP_HOME="${RUSTUP_HOME:-/home/gema/workspace/.gema/home/.rustup}"
  export HF_HOME="${HF_HOME:-/home/gema/workspace/.gema/cache/huggingface}"
  export TRANSFORMERS_CACHE="${TRANSFORMERS_CACHE:-/home/gema/workspace/.gema/cache/huggingface/transformers}"
  export TORCH_HOME="${TORCH_HOME:-/home/gema/workspace/.gema/cache/torch}"
  export GEMA_APT_PREFIX="${GEMA_APT_PREFIX:-/home/gema/workspace/.gema/apt}"
  case ":$PATH:" in
    *":$GEMA_APT_PREFIX/root/usr/bin:"*) ;;
    *) export PATH="$GEMA_APT_PREFIX/root/usr/local/sbin:$GEMA_APT_PREFIX/root/usr/local/bin:$GEMA_APT_PREFIX/root/usr/sbin:$GEMA_APT_PREFIX/root/usr/bin:$PATH" ;;
  esac
  export LD_LIBRARY_PATH="$GEMA_APT_PREFIX/root/usr/local/lib:$GEMA_APT_PREFIX/root/usr/lib:$GEMA_APT_PREFIX/root/usr/lib/x86_64-linux-gnu:$GEMA_APT_PREFIX/root/lib:$GEMA_APT_PREFIX/root/lib/x86_64-linux-gnu:${LD_LIBRARY_PATH:-}"
  export PKG_CONFIG_PATH="$GEMA_APT_PREFIX/root/usr/local/lib/pkgconfig:$GEMA_APT_PREFIX/root/usr/lib/pkgconfig:$GEMA_APT_PREFIX/root/usr/lib/x86_64-linux-gnu/pkgconfig:${PKG_CONFIG_PATH:-}"
  export CMAKE_PREFIX_PATH="$GEMA_APT_PREFIX/root/usr:$GEMA_APT_PREFIX/root/usr/local:${CMAKE_PREFIX_PATH:-}"
  export CPATH="$GEMA_APT_PREFIX/root/usr/include:$GEMA_APT_PREFIX/root/usr/include/x86_64-linux-gnu:${CPATH:-}"
fi
cd "$HOME"
exec /bin/bash -l
EOF
  chmod 0755 "$ROOTFS/usr/local/bin/gema-terminal"
  mkdir -p "$ROOTFS/usr/share/applications"
  cat > "$ROOTFS/usr/share/applications/org.gnome.Terminal.desktop" <<'EOF'
[Desktop Entry]
Name=Terminal
Comment=Use the command line
Keywords=shell;prompt;command;commandline;cmd;
TryExec=gema-terminal
Exec=gema-terminal
Icon=org.gnome.Terminal
Type=Application
Categories=GNOME;GTK;System;TerminalEmulator;
StartupNotify=true
StartupWMClass=XTerm
X-Ubuntu-Gettext-Domain=gnome-terminal
EOF
  if [ -d "$GEMA_CLOUD_DIR/agent/dist" ]; then
    rm -rf "$ROOTFS/opt/gema-agent/dist"
    mkdir -p "$ROOTFS/opt/gema-agent"
    copy_if_present "$GEMA_CLOUD_DIR/agent/package.json" "$ROOTFS/opt/gema-agent/package.json" 0644
    cp -a "$GEMA_CLOUD_DIR/agent/dist" "$ROOTFS/opt/gema-agent/dist"
    if [ -d "$GEMA_CLOUD_DIR/agent/node_modules" ]; then
      rm -rf "$ROOTFS/opt/gema-agent/node_modules"
      cp -a "$GEMA_CLOUD_DIR/agent/node_modules" "$ROOTFS/opt/gema-agent/node_modules"
    fi
    find "$ROOTFS/opt/gema-agent/dist" -type f \( -name '*.js' -o -name 'bin.js' -o -name 'bridge.js' \) -exec chmod 0755 {} +
  fi

  chroot "$ROOTFS" /bin/bash -lc '
    set -euo pipefail
    if ! id -u gema >/dev/null 2>&1; then
      useradd -m -u 1000 -s /bin/bash gema
    fi
    passwd -l root >/dev/null || true
    passwd -l gema >/dev/null || true
    chown -R 1000:1000 /home/gema
  '
}

prune_cuda_toolkit_artifacts() {
  if [ "$INSTALL_CUDA_TOOLKITS" = "1" ]; then
    return 0
  fi
  echo "[gema-rootfs] pruning CUDA toolkit apt artifacts for wheel-only image"
  mount_for_chroot
  chroot "$ROOTFS" /bin/bash -lc '
    set -euo pipefail
    DEBIAN_FRONTEND=noninteractive dpkg --configure -a
    mapfile -t packages < <(dpkg-query -W -f='"'"'${binary:Package}\n'"'"' 2>/dev/null | sed "s/:.*//" | sort -u | grep -E "^(cuda-|libcudnn|libnccl|gds-tools|nsight-|nvidia-cuda-toolkit|nvidia-opencl-dev|libcublas|libcufft|libcufile|libcurand|libcusolver|libcusparse|libcuobjclient|libnpp|libnvfatbin|libnvjitlink|libnvjpeg|libnvptxcompiler|libnvvm)" || true)
    if [ "${#packages[@]}" -gt 0 ]; then
      DEBIAN_FRONTEND=noninteractive apt-get purge -y -qq "${packages[@]}"
      DEBIAN_FRONTEND=noninteractive apt-get autoremove -y -qq --purge
    fi
    rm -rf /usr/local/cuda /usr/local/cuda-* /opt/nvidia /opt/gema/cuda
    rm -f /etc/apt/sources.list.d/cuda*.list /etc/apt/preferences.d/cuda-repository-pin-600
    rm -f /etc/ld.so.conf.d/*cuda*.conf
    update-alternatives --remove-all cuda >/dev/null 2>&1 || true
    update-alternatives --remove-all cuda-12 >/dev/null 2>&1 || true
    update-alternatives --remove-all cuda-13 >/dev/null 2>&1 || true
    ldconfig >/dev/null 2>&1 || true
  '
}

mount_for_chroot() {
  mkdir -p "$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/dev/pts"
  if ! mountpoint -q "$ROOTFS/proc"; then
    mount -t proc proc "$ROOTFS/proc"
    BUILD_MOUNTS+=("$ROOTFS/proc")
  fi
  if ! mountpoint -q "$ROOTFS/sys"; then
    mount --rbind /sys "$ROOTFS/sys"
    mount --make-rslave "$ROOTFS/sys"
    BUILD_MOUNTS+=("$ROOTFS/sys")
  fi
  if ! mountpoint -q "$ROOTFS/dev"; then
    mount --rbind /dev "$ROOTFS/dev"
    mount --make-rslave "$ROOTFS/dev"
    BUILD_MOUNTS+=("$ROOTFS/dev")
  fi
}

install_ml_stack() {
  if [ ! -d "$ROOTFS" ]; then
    echo "ERROR: rootfs missing for ML install: $ROOTFS" >&2
    exit 66
  fi
  if [ ! -f "$ML_REQUIREMENTS" ]; then
    echo "ERROR: ML requirements missing: $ML_REQUIREMENTS" >&2
    exit 66
  fi
  if [ ! -f "$CUDA_LANES_FILE" ]; then
    echo "ERROR: CUDA lane manifest missing: $CUDA_LANES_FILE" >&2
    exit 66
  fi
  if [ ! -f "$CUDA_SELECT_SCRIPT" ]; then
    echo "ERROR: CUDA selector missing: $CUDA_SELECT_SCRIPT" >&2
    exit 66
  fi
  if [ ! -f "$ML_RECOVERY_POLICIES" ]; then
    echo "ERROR: ML recovery policies missing: $ML_RECOVERY_POLICIES" >&2
    exit 66
  fi
  local requirements_sha stack_sha
  if [ ! -f "$FABRIC_BRIDGE_SCRIPT" ]; then
    echo "ERROR: fabric bridge missing: $FABRIC_BRIDGE_SCRIPT" >&2
    exit 66
  fi
  if [ ! -f "$ML_EXPANSION_REQUIREMENTS" ]; then
    echo "ERROR: ML expansion requirements missing: $ML_EXPANSION_REQUIREMENTS" >&2
    exit 66
  fi
  requirements_sha="$(sha256sum "$ML_REQUIREMENTS" "$ML_EXPANSION_REQUIREMENTS" "$ML_RECOVERY_POLICIES" "$CUDA_LANES_FILE" "$CUDA_SELECT_SCRIPT" "$FABRIC_BRIDGE_SCRIPT" | sha256sum | awk '{print $1}')"
  stack_sha="$(printf '%s|image=%s|profile=%s|lanes=%s|default=%s|toolkits=%s|strict=%s|policy=%s|prune=%s|toolkit_packages=%s|optional=%s\n' "$requirements_sha" "$IMAGE_ID" "$IMAGE_PROFILE" "$DEFAULT_ML_LANES" "$DEFAULT_CUDA_LANE" "$INSTALL_CUDA_TOOLKITS" "$STRICT_LANE_POLICY" "$LANE_POLICY_VERSION" "$WHEEL_ONLY_PRUNE_VERSION" "$CUDA_TOOLKIT_PACKAGES" "$CUDA_OPTIONAL_PACKAGES" | sha256sum | awk '{print $1}')"
  if [ -f "$ROOTFS/.gema-ml-ready" ] &&
    [ -f "$ROOTFS/opt/gema/ml-requirements.txt" ] &&
    [ -f "$ROOTFS/opt/gema/cuda-lanes.json" ] &&
    [ "$(cat "$ROOTFS/.gema-ml-ready" 2>/dev/null || true)" = "$stack_sha" ]; then
    echo "[gema-rootfs] ml ready: $ROOTFS"
    return 0
  fi

  echo "[gema-rootfs] installing multi-lane ML stack"
  configure_apt_sources
  mount_for_chroot
  chroot "$ROOTFS" env DEBIAN_FRONTEND=noninteractive dpkg --configure -a
  chroot "$ROOTFS" env DEBIAN_FRONTEND=noninteractive apt-get update
  chroot "$ROOTFS" env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    ca-certificates curl python3 python3-full python3-dev python3-venv python3-pip \
    ffmpeg openjdk-17-jre-headless build-essential pkg-config git gnupg \
    openssh-client openssh-server jq wget rsync tmux htop nano nodejs npm
  install_cuda_toolkits
  prune_cuda_toolkit_artifacts
  mkdir -p "$ROOTFS/opt/gema/bin" "$ROOTFS/opt/gema/ml" "$ROOTFS/opt/gema/cuda"
  install -m 0644 "$ML_REQUIREMENTS" "$ROOTFS/opt/gema/ml-requirements.txt"
  install -m 0644 "$ML_EXPANSION_REQUIREMENTS" "$ROOTFS/opt/gema/ml-expansion-requirements.txt"
  install -m 0644 "$ML_RECOVERY_POLICIES" "$ROOTFS/opt/gema/ml-recovery-policies.json"
  install -m 0644 "$CUDA_LANES_FILE" "$ROOTFS/opt/gema/cuda-lanes.json"
  install -m 0755 "$CUDA_SELECT_SCRIPT" "$ROOTFS/opt/gema/bin/gema-cuda-select"
  install -m 0755 "$FABRIC_BRIDGE_SCRIPT" "$ROOTFS/opt/gema/bin/gema-fabric-bridge"
  install -m 0755 "$FABRIC_BRIDGE_SCRIPT" "$ROOTFS/opt/gema/bin/gema-ml-bridge"
  install -m 0755 "$FABRIC_BRIDGE_SCRIPT" "$ROOTFS/opt/gema/bin/gema-accelerator-worker"
  install -m 0755 "$FABRIC_BRIDGE_SCRIPT" "$ROOTFS/opt/gema/bin/gema-cluster-bridge"
  install_cuda_proxy
  install_gvphase1_cuda_proxy

  if [ -x "$ROOTFS/opt/gema/python/bin/python" ] && [ ! -x "$ROOTFS/opt/gema/ml/cu121/bin/python" ]; then
    echo "[gema-rootfs] migrating legacy /opt/gema/python to /opt/gema/ml/cu121"
    mv "$ROOTFS/opt/gema/python" "$ROOTFS/opt/gema/ml/cu121"
    chroot "$ROOTFS" /opt/gema/ml/cu121/bin/python -m pip freeze > "$ROOTFS/opt/gema/ml/cu121/gema.freeze.txt" || true
    printf 'cu121\n' > "$ROOTFS/opt/gema/ml/cu121/.gema-lane-ready"
  fi

  local lane
  for lane in $DEFAULT_ML_LANES; do
    install_python_lane "$lane" "$stack_sha"
  done

  cat > "$ROOTFS/opt/gema/bin/python" <<EOF
#!/bin/sh
env_dir="\$(/opt/gema/bin/gema-cuda-select --env)"
eval "\$(/opt/gema/bin/gema-cuda-select --exports)"
export GEMA_CUDA_LANE_SELECTED VIRTUAL_ENV PATH CUDA_HOME CUDA_PATH LD_LIBRARY_PATH
exec "\$env_dir/bin/python" "\$@"
EOF
  cat > "$ROOTFS/opt/gema/bin/python3" <<EOF
#!/bin/sh
exec /opt/gema/bin/python "\$@"
EOF
  cat > "$ROOTFS/opt/gema/bin/pip" <<EOF
#!/bin/sh
env_dir="\$(/opt/gema/bin/gema-cuda-select --env)"
eval "\$(/opt/gema/bin/gema-cuda-select --exports)"
export GEMA_CUDA_LANE_SELECTED VIRTUAL_ENV PATH CUDA_HOME CUDA_PATH LD_LIBRARY_PATH
exec "\$env_dir/bin/python" -m pip "\$@"
EOF
  cat > "$ROOTFS/opt/gema/bin/pip3" <<EOF
#!/bin/sh
exec /opt/gema/bin/pip "\$@"
EOF
  cat > "$ROOTFS/opt/gema/bin/gema-gpu-ready-gate" <<'EOF'
#!/bin/sh
set -eu
if [ "${GEMA_CUDA_PROXY_READY:-0}" = "1" ]; then
  exit 0
fi
workspace="${GEMA_PROJECT_DIR:-${GEMA_WORKSPACE:-$PWD}}"
python="${PYTHON:-/usr/bin/python3}"
"$python" - "$workspace" <<'PY'
import json
import pathlib
import sys
workspace = pathlib.Path(sys.argv[1])
for rel in (".gema/fabric/cuda_proxy_state.json", ".gema/agent/fabric/cuda_proxy_state.json"):
    path = workspace / rel
    try:
        state = json.loads(path.read_text())
    except Exception:
        continue
    if state.get("ready") is True and state.get("status") == "validated":
        raise SystemExit(0)
raise SystemExit(79)
PY
EOF
  cat > "$ROOTFS/opt/gema/bin/gema-gpu-command" <<'EOF'
#!/bin/sh
set -eu
cmd="$(basename "$0")"
if ! /opt/gema/bin/gema-gpu-ready-gate; then
  echo "gema: GPU command '$cmd' is blocked until cuda_proxy.ready=true" >&2
  exit 79
fi
env_dir="$(timeout 3 /opt/gema/bin/gema-cuda-select --env 2>/dev/null || true)"
eval "$(timeout 3 /opt/gema/bin/gema-cuda-select --exports 2>/dev/null || true)"
export GEMA_CUDA_LANE_SELECTED VIRTUAL_ENV PATH CUDA_HOME CUDA_PATH LD_LIBRARY_PATH
for dir in "$env_dir/bin" /usr/local/bin /usr/bin /bin; do
  target="$dir/$cmd"
  if [ -x "$target" ] && [ "$target" != "$0" ]; then
    exec "$target" "$@"
  fi
done
echo "gema: GPU command '$cmd' is ready-gated but no backend executable was found" >&2
exit 127
EOF
  local gpu_cmd
  for gpu_cmd in torchrun deepspeed accelerate vllm text-generation-launcher text-generation-server tritonserver nvidia-smi; do
    ln -sf gema-gpu-command "$ROOTFS/opt/gema/bin/$gpu_cmd"
  done
  chmod 0755 "$ROOTFS/opt/gema/bin/python" "$ROOTFS/opt/gema/bin/python3" "$ROOTFS/opt/gema/bin/pip" "$ROOTFS/opt/gema/bin/pip3" "$ROOTFS/opt/gema/bin/gema-gpu-ready-gate" "$ROOTFS/opt/gema/bin/gema-gpu-command"
  chroot "$ROOTFS" chown -R 1000:1000 /opt/gema/ml /opt/gema/bin
  printf '%s\n' "$stack_sha" > "$ROOTFS/.gema-ml-ready"
}

torch_index_for_lane() {
  case "$1" in
    cpu) printf 'https://download.pytorch.org/whl/cpu\n' ;;
    cu118) printf 'https://download.pytorch.org/whl/cu118\n' ;;
    cu121) printf 'https://download.pytorch.org/whl/cu121\n' ;;
    cu124) printf 'https://download.pytorch.org/whl/cu124\n' ;;
    *) echo "ERROR: unsupported Python CUDA lane: $1" >&2; exit 66 ;;
  esac
}

torch_spec_for_lane() {
  case "$1" in
    cpu) printf 'torch==2.5.1 torchvision==0.20.1 torchaudio==2.5.1\n' ;;
    cu118) printf 'torch==2.5.1+cu118 torchvision==0.20.1+cu118 torchaudio==2.5.1+cu118\n' ;;
    cu121) printf 'torch==2.5.1+cu121 torchvision==0.20.1+cu121 torchaudio==2.5.1+cu121\n' ;;
    cu124) printf 'torch==2.5.1+cu124 torchvision==0.20.1+cu124 torchaudio==2.5.1+cu124\n' ;;
    *) echo "ERROR: unsupported Python CUDA lane: $1" >&2; exit 66 ;;
  esac
}

lane_ready() {
  local lane="$1"
  local env_path="$ML_ROOT/$lane"
  local expected="$2"
  local current
  if [ ! -x "$ROOTFS$env_path/bin/python" ] || [ ! -f "$ROOTFS$env_path/.gema-lane-ready" ]; then
    return 1
  fi
  current="$(chroot "$ROOTFS" "$env_path/bin/python" - <<'PY' 2>/dev/null || true
import importlib.metadata as md
packages = ("torch", "torchvision", "torchaudio")
print(" ".join(f"{name}=={md.version(name)}" for name in packages))
PY
)"
  [ "$current" = "$expected" ]
}

lane_stack_ready() {
  local lane="$1"
  local env_path="$ML_ROOT/$lane"
  local expected="$2"
  [ -f "$ROOTFS$env_path/.gema-lane-ready" ] &&
    [ "$(cat "$ROOTFS$env_path/.gema-lane-ready" 2>/dev/null || true)" = "$expected" ]
}

write_lane_constraints() {
  local lane="$1"
  local env_path="$ML_ROOT/$lane"
  chroot "$ROOTFS" "$env_path/bin/python" - <<PY > "$ROOTFS$env_path/gema.constraints.txt"
import importlib.metadata as md

for name in ("numpy", "torch", "torchvision", "torchaudio"):
    try:
        print(f"{name}=={md.version(name)}")
    except md.PackageNotFoundError:
        pass
PY
}

install_lane_requirements() {
  local lane="$1"
  local env_path="$ML_ROOT/$lane"
  local core_requirements="/opt/gema/ml-requirements.txt"
  if [ "$lane" = "cpu" ]; then
    return 0
  fi
  if [ "$lane" = "cu118" ] && [ "$STRICT_LANE_POLICY" = "1" ]; then
    grep -vE '^vllm==' "$ROOTFS/opt/gema/ml-requirements.txt" > "$ROOTFS$env_path/gema.core-requirements.txt"
    core_requirements="$env_path/gema.core-requirements.txt"
  fi
  write_lane_constraints "$lane"
  chroot "$ROOTFS" "$env_path/bin/python" -m pip install --disable-pip-version-check --no-cache-dir --upgrade-strategy only-if-needed -c "$env_path/gema.constraints.txt" -r "$core_requirements"
  write_lane_constraints "$lane"
  chroot "$ROOTFS" "$env_path/bin/python" -m pip install --disable-pip-version-check --no-cache-dir --upgrade-strategy only-if-needed -c "$env_path/gema.constraints.txt" -r /opt/gema/ml-expansion-requirements.txt
}

apply_lane_policy() {
  local lane="$1"
  local env_path="$ML_ROOT/$lane"
  mkdir -p "$ROOTFS$env_path"
  case "$lane" in
    cu118)
      if [ "$STRICT_LANE_POLICY" = "1" ]; then
        chroot "$ROOTFS" "$env_path/bin/python" -m pip uninstall -y vllm >/dev/null 2>&1 || true
        cat > "$ROOTFS$env_path/gema.lane-policy.json" <<'EOF'
{
  "schema": "gema.ml_lane_policy.v1",
  "unsupported": {
    "vllm": "The available vLLM wheels for this stack are CUDA 12-linked; the cu118 lane intentionally does not ship a broken CUDA 11.8 vLLM extension."
  }
}
EOF
      fi
      ;;
    *)
      cat > "$ROOTFS$env_path/gema.lane-policy.json" <<'EOF'
{
  "schema": "gema.ml_lane_policy.v1",
  "unsupported": {}
}
EOF
      ;;
  esac
}

install_python_lane() {
  local lane="$1"
  local stack_sha="$2"
  local env_path="$ML_ROOT/$lane"
  local torch_index
  local torch_spec
  torch_index="$(torch_index_for_lane "$lane")"
  torch_spec="$(torch_spec_for_lane "$lane")"
  if lane_ready "$lane" "$torch_spec"; then
    if ! lane_stack_ready "$lane" "$torch_spec|$stack_sha"; then
      echo "[gema-rootfs] refreshing ML lane requirements: $lane"
      install_lane_requirements "$lane"
    fi
    apply_lane_policy "$lane"
    chroot "$ROOTFS" "$env_path/bin/python" -m pip freeze > "$ROOTFS$env_path/gema.freeze.txt"
    printf '%s\n' "$torch_spec|$stack_sha" > "$ROOTFS$env_path/.gema-lane-ready"
    echo "[gema-rootfs] ML lane ready: $lane"
    return 0
  fi
  echo "[gema-rootfs] installing ML lane: $lane"
  rm -rf "$ROOTFS$env_path"
  chroot "$ROOTFS" python3 -m venv "$env_path"
  chroot "$ROOTFS" "$env_path/bin/python" -m ensurepip --upgrade
  chroot "$ROOTFS" "$env_path/bin/python" -m pip install --disable-pip-version-check --no-cache-dir --upgrade pip setuptools wheel
  chroot "$ROOTFS" "$env_path/bin/python" -m pip install --disable-pip-version-check --no-cache-dir --index-url "$torch_index" $torch_spec
  install_lane_requirements "$lane"
  chroot "$ROOTFS" "$env_path/bin/python" -m pip install --disable-pip-version-check --no-cache-dir --index-url "$torch_index" $torch_spec
  apply_lane_policy "$lane"
  chroot "$ROOTFS" "$env_path/bin/python" -m pip freeze > "$ROOTFS$env_path/gema.freeze.txt"
  printf '%s\n' "$torch_spec|$stack_sha" > "$ROOTFS$env_path/.gema-lane-ready"
}

write_gema_release() {
  local ml_requirements="0"
  local cuda_lanes="0"
  if [ -f "$ROOTFS/.gema-ml-ready" ] || [ "$INSTALL_ML_STACK" = "1" ]; then
    ml_requirements="1"
    cuda_lanes="1"
  fi
  cat > "$ROOTFS/etc/gema-release" <<EOF
NAME="Gema Linux Substrate"
BASE="Ubuntu $SUITE"
GEMA_NATIVE="1"
GEMA_MANAGED_WORKSPACE="$INSTALL_MANAGED_WORKSPACE"
GEMA_ML_REQUIREMENTS="$ml_requirements"
GEMA_CUDA_LANES="$cuda_lanes"
EOF
}

write_image_manifest() {
  python3 - "$ROOTFS" "$SUITE" "$ARCH" "$IMAGE_ID" "$IMAGE_PROFILE" "$DEFAULT_CUDA_LANE" <<'PY'
import hashlib
import json
import os
import sys
import time

rootfs, suite, arch, image_id, image_profile, default_cuda_lane = sys.argv[1:7]

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

def installed_ml_lanes():
    lanes = {}
    base = os.path.join(rootfs, "opt/gema/ml")
    if not os.path.isdir(base):
        return lanes
    for lane in sorted(os.listdir(base)):
        python = os.path.join(base, lane, "bin/python")
        if os.path.exists(python):
            lanes[lane] = {
                "python": f"/opt/gema/ml/{lane}/bin/python",
                "freeze": os.path.exists(os.path.join(base, lane, "gema.freeze.txt")),
                "policy": os.path.exists(os.path.join(base, lane, "gema.lane-policy.json")),
            }
    return lanes

def installed_cuda_toolkits():
    values = []
    base = os.path.join(rootfs, "usr/local")
    if os.path.isdir(base):
        for name in sorted(os.listdir(base)):
            if name.startswith("cuda-") and os.path.isdir(os.path.join(base, name)):
                values.append(name.removeprefix("cuda-"))
    return values

digest = hashlib.sha256()
for relative in ("etc/os-release", "etc/gema-release", ".gema-rootfs-ready", ".gema-ml-ready", "opt/gema/ml-requirements.txt", "opt/gema/ml-expansion-requirements.txt", "opt/gema/ml-recovery-policies.json", "opt/gema/cuda-lanes.json", "var/lib/dpkg/status"):
    digest.update(relative.encode())
    digest.update(read(relative))

os_release = kv("etc/os-release")
ready = read(".gema-rootfs-ready").decode("utf-8", errors="replace").strip() or time.strftime("%Y-%m-%dT%H:%M:%S%z")
toolkits = installed_cuda_toolkits()
lanes = installed_ml_lanes()
manifest = {
    "id": image_id,
    "name": "Gema Linux Substrate",
    "profile": image_profile,
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
        "commands": {name: command_exists(name) for name in ("bash", "python3", "curl", "ip", "sudo", "ssh", "sshd", "ssh-keygen", "ffmpeg", "git", "jq", "wget", "rsync", "tmux", "htop", "nano", "node", "npm", "gema-cuda-select", "gema-fabric-bridge", "gema-ml-bridge", "gema-accelerator-worker", "gema-cluster-bridge", "gvphase1-backend")},
        "workspace_ui": {"browser": "gema-session-shell", "desktop_streaming": False, "terminal": "managed-api"},
        "gpu": {"nvidia_userland": command_exists("nvidia-smi") or bool(toolkits), "cuda_toolkits": toolkits, "cuda_path": "/usr/local/cuda" if os.path.exists(os.path.join(rootfs, "usr/local/cuda")) else None, "driver_required_from_host": bool(lanes)},
        "cuda_proxy": {"primary": os.path.exists(os.path.join(rootfs, "opt/gema/gvphase1/lib/libcuda.so.1")) and os.path.exists(os.path.join(rootfs, "opt/gema/gvphase1/bin/gvphase1-backend")), "kind": "gvphase1_binary_cuda_api_remoting" if os.path.exists(os.path.join(rootfs, "opt/gema/gvphase1/lib/libcuda.so.1")) else ("legacy_driver_rpc_prototype" if os.path.exists(os.path.join(rootfs, "opt/gema/cuda-proxy/lib/libcuda.so.1")) else "none"), "frontend_library": "/opt/gema/gvphase1/lib/libcuda.so.1" if os.path.exists(os.path.join(rootfs, "opt/gema/gvphase1/lib/libcuda.so.1")) else None, "backend": "/opt/gema/gvphase1/bin/gvphase1-backend" if os.path.exists(os.path.join(rootfs, "opt/gema/gvphase1/bin/gvphase1-backend")) else None, "truth_role": "primary data plane for terminal-visible remote GPU after backend validation"},
        "ml": {"python3": command_exists("python3"), "selector": command_exists("gema-cuda-select"), "fabric_bridge": command_exists("gema-fabric-bridge"), "bridge": command_exists("gema-ml-bridge"), "accelerator_worker": command_exists("gema-accelerator-worker"), "cluster_bridge": command_exists("gema-cluster-bridge"), "managed_env_root": "/opt/gema/ml", "default_lane": default_cuda_lane, "wheel_only": bool(lanes) and not bool(toolkits), "lanes": lanes, "requirements": os.path.exists(os.path.join(rootfs, "opt/gema/ml-requirements.txt")), "expansion_requirements": os.path.exists(os.path.join(rootfs, "opt/gema/ml-expansion-requirements.txt")), "recovery_policies": os.path.exists(os.path.join(rootfs, "opt/gema/ml-recovery-policies.json")), "domains": ["rl", "audio_speech", "vision", "graph_ml", "classical_ml", "dataset_versioning", "lakehouse", "onnx_runtime_gpu"] if lanes else [], "jupyter": any(os.path.exists(os.path.join(rootfs, "opt/gema/ml", lane, "bin/jupyter")) for lane in lanes)},
    },
    "compatibility": {
        "runtime": ["gema-native"],
        "gpu_vendors": ["cpu", "nvidia"] if lanes else ["cpu"],
        "requires": {"min_nvidia_driver": None, "cuda": {"python_lanes": sorted(lanes), "toolkits": toolkits}, "kernel_features": ["cgroup-v2", "user-namespace", "network-namespace"]},
    },
    "generated_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
}
with open(os.path.join(rootfs, "etc/gema-image.json"), "w", encoding="utf-8") as handle:
    json.dump(manifest, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY
}

if [ -f "$ROOTFS/.gema-rootfs-ready" ]; then
  if [ "$METADATA_ONLY" != "1" ]; then
    install_managed_workspace_stack
  fi
  if [ "$METADATA_ONLY" != "1" ] && [ "$INSTALL_ML_STACK" = "1" ]; then
    install_ml_stack
  fi
  write_gema_release
  write_image_manifest
  echo "[gema-rootfs] ready: $ROOTFS"
  exit 0
fi

mkdir -p "$(dirname "$ROOTFS")"
echo "[gema-rootfs] building suite=$SUITE arch=$ARCH rootfs=$ROOTFS"
debootstrap \
  --arch="$ARCH" \
  --variant=minbase \
  --include=ca-certificates,curl,bash,coreutils,findutils,procps,iproute2,iputils-ping,less,vim-tiny,util-linux,python3,openssh-client,openssh-server \
  "$SUITE" "$ROOTFS" "$MIRROR"

cat > "$ROOTFS/etc/hostname" <<'EOF'
gema-substrate
EOF

cat > "$ROOTFS/etc/hosts" <<'EOF'
127.0.0.1 localhost
127.0.1.1 gema-substrate
EOF

if ! chroot "$ROOTFS" id -u gema >/dev/null 2>&1; then
  chroot "$ROOTFS" useradd -m -u 1000 -s /bin/bash gema
fi
chroot "$ROOTFS" passwd -l root >/dev/null
chroot "$ROOTFS" passwd -l gema >/dev/null

mkdir -p "$ROOTFS/home/gema/workspace" "$ROOTFS/home/gema/.gema/agent" "$ROOTFS/run" "$ROOTFS/tmp"
chown -R 1000:1000 "$ROOTFS/home/gema"
chmod 1777 "$ROOTFS/tmp"

install_managed_workspace_stack
if [ "$INSTALL_ML_STACK" = "1" ]; then
  install_ml_stack
fi

write_gema_release
date -Is > "$ROOTFS/.gema-rootfs-ready"
write_image_manifest
echo "[gema-rootfs] built: $ROOTFS"
