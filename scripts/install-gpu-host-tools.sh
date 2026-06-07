#!/usr/bin/env bash
set -euo pipefail

FORCE_NVIDIA=0
FORCE_AMD=0
INSTALL_DRIVERS=0

for arg in "$@"; do
  case "$arg" in
    --force-nvidia) FORCE_NVIDIA=1 ;;
    --force-amd) FORCE_AMD=1 ;;
    --install-drivers) INSTALL_DRIVERS=1 ;;
    *)
      echo "usage: $0 [--force-nvidia] [--force-amd] [--install-drivers]" >&2
      exit 64
      ;;
  esac
done

if [ "$(id -u)" != "0" ]; then
  echo "ERROR: run as root" >&2
  exit 77
fi

. /etc/os-release
if [ "${ID:-}" != "ubuntu" ]; then
  echo "ERROR: this installer currently supports Ubuntu hosts only" >&2
  exit 78
fi

has_nvidia=0
has_amd=0
gpu_lines="$(lspci | grep -Ei 'vga|3d|display' || true)"
if printf '%s\n' "$gpu_lines" | grep -Eiq 'nvidia'; then
  has_nvidia=1
fi
if printf '%s\n' "$gpu_lines" | grep -Eiq 'advanced micro devices|amd/ati|ati technologies| radeon | instinc?t | mi[0-9]'; then
  has_amd=1
fi
[ "$FORCE_NVIDIA" = "1" ] && has_nvidia=1
[ "$FORCE_AMD" = "1" ] && has_amd=1

echo "[gema-runtime] host=${PRETTY_NAME:-ubuntu} kernel=$(uname -r)"
echo "[gema-runtime] nvidia_detected=${has_nvidia} amd_detected=${has_amd}"

apt-get update
apt-get install -y ca-certificates curl gnupg lsb-release pciutils

install_nvidia_stack() {
  echo "[gema-runtime] installing NVIDIA host tooling"
  if [ "$INSTALL_DRIVERS" = "1" ]; then
    apt-get install -y ubuntu-drivers-common
    ubuntu-drivers install --gpgpu || ubuntu-drivers autoinstall || true
  fi

  install -m 0755 -d /usr/share/keyrings
  curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey \
    | gpg --dearmor -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg

  distribution=$(. /etc/os-release; echo "${ID}${VERSION_ID}")
  if ! curl -fsSL "https://nvidia.github.io/libnvidia-container/${distribution}/libnvidia-container.list" >/tmp/nvidia-container-toolkit.list; then
    echo "[gema-runtime] NVIDIA toolkit repo missing for ${distribution}; falling back to ubuntu22.04 repo"
    curl -fsSL "https://nvidia.github.io/libnvidia-container/ubuntu22.04/libnvidia-container.list" >/tmp/nvidia-container-toolkit.list
  fi
  sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' \
    /tmp/nvidia-container-toolkit.list > /etc/apt/sources.list.d/nvidia-container-toolkit.list

  apt-get update
  apt-get install -y nvidia-container-toolkit

  if command -v nvidia-ctk >/dev/null 2>&1; then
    nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml || true
    nvidia-ctk runtime configure --runtime=docker || true
  fi
  systemctl restart docker 2>/dev/null || true

  if command -v nvidia-smi >/dev/null 2>&1; then
    nvidia-smi || true
  else
    echo "[gema-runtime] nvidia-smi not present yet. Reboot may be required after driver install."
  fi
}

install_amd_stack() {
  echo "[gema-runtime] installing AMD ROCm host tooling"
  apt-get install -y rocm-smi pciutils || apt-get install -y rocm-smi-lib pciutils || true
  if command -v rocm-smi >/dev/null 2>&1; then
    rocm-smi || true
  else
    echo "[gema-runtime] rocm-smi unavailable from current apt sources. Add AMD ROCm repo for full GPU hosts."
  fi
}

if [ "$has_nvidia" = "1" ]; then
  install_nvidia_stack
else
  echo "[gema-runtime] skipping NVIDIA install: no NVIDIA GPU detected. Use --force-nvidia on GPU hosts if detection is hidden."
fi

if [ "$has_amd" = "1" ]; then
  install_amd_stack
else
  echo "[gema-runtime] skipping AMD install: no AMD GPU detected. Use --force-amd on GPU hosts if detection is hidden."
fi

echo "[gema-runtime] final probe"
if [ -x ./target/debug/gema-runtime ]; then
  ./target/debug/gema-runtime probe || true
  ./target/debug/gema-runtime gpu || true
fi
