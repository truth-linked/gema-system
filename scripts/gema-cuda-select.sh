#!/usr/bin/env bash
set -euo pipefail

ROOT="${GEMA_ROOTFS_PREFIX:-}"
LANES_DIR="$ROOT/opt/gema/ml"
DEFAULT_LANE="${GEMA_DEFAULT_CUDA_LANE:-cu121}"

lane_cuda() {
  case "$1" in
    cu118) printf '11.8\n' ;;
    cu121) printf '12.1\n' ;;
    cu124) printf '12.4\n' ;;
    cu126) printf '12.6\n' ;;
    cu128) printf '12.8\n' ;;
    cu129) printf '12.9\n' ;;
    cu130) printf '13.0\n' ;;
    cu131) printf '13.1\n' ;;
    cu132) printf '13.2\n' ;;
    cpu) printf '0\n' ;;
    *) printf '0\n' ;;
  esac
}

cuda_home_for_lane() {
  case "$1" in
    cu118) printf '/usr/local/cuda-11.8\n' ;;
    cu121) printf '/usr/local/cuda-12.1\n' ;;
    cu124) printf '/usr/local/cuda-12.4\n' ;;
    cu126) printf '/usr/local/cuda-12.6\n' ;;
    cu128) printf '/usr/local/cuda-12.8\n' ;;
    cu129) printf '/usr/local/cuda-12.9\n' ;;
    cu130) printf '/usr/local/cuda-13.0\n' ;;
    cu131) printf '/usr/local/cuda-13.1\n' ;;
    cu132) printf '/usr/local/cuda-13.2\n' ;;
    *) printf '\n' ;;
  esac
}

wheel_library_path() {
  local root_env="$ROOT$env_dir"
  local python_dir site_dir dir rel
  local paths=""
  if [ -d "$root_env/lib" ]; then
    python_dir="$(find "$root_env/lib" -maxdepth 1 -type d -name 'python*' | sort | head -n1 || true)"
    site_dir="$python_dir/site-packages"
    if [ -d "$site_dir" ]; then
      for dir in "$site_dir/torch/lib" "$site_dir"/nvidia/*/lib; do
        [ -d "$dir" ] || continue
        rel="${dir#$ROOT}"
        if [ -z "$paths" ]; then
          paths="$rel"
        else
          paths="$paths:$rel"
        fi
      done
    fi
  fi
  printf '%s\n' "$paths"
}

version_le() {
  awk -v a="$1" -v b="$2" 'BEGIN {
    split(a, av, "."); split(b, bv, ".");
    amaj = av[1] + 0; amin = av[2] + 0;
    bmaj = bv[1] + 0; bmin = bv[2] + 0;
    exit !((amaj < bmaj) || (amaj == bmaj && amin <= bmin));
  }'
}

host_cuda_version() {
  if [ -n "${GEMA_HOST_CUDA_VERSION:-}" ]; then
    printf '%s\n' "$GEMA_HOST_CUDA_VERSION"
    return 0
  fi
  if command -v nvidia-smi >/dev/null 2>&1; then
    nvidia-smi --query-gpu=cuda_version --format=csv,noheader 2>/dev/null | head -n1 | tr -d ' ' || true
    return 0
  fi
  printf '\n'
}

lane_exists() {
  [ -x "$LANES_DIR/$1/bin/python" ]
}

select_lane() {
  local requested="${GEMA_CUDA_LANE:-auto}"
  if [ "$requested" != "auto" ] && [ -n "$requested" ]; then
    if lane_exists "$requested"; then
      printf '%s\n' "$requested"
      return 0
    fi
    echo "ERROR: requested CUDA lane is not installed: $requested" >&2
    exit 66
  fi

  local host_cuda selected lane lane_ver
  host_cuda="$(host_cuda_version)"
  selected=""
  if [ -n "$host_cuda" ]; then
    for lane in cu132 cu131 cu130 cu129 cu128 cu126 cu124 cu121 cu118; do
      lane_exists "$lane" || continue
      lane_ver="$(lane_cuda "$lane")"
      if version_le "$lane_ver" "$host_cuda"; then
        selected="$lane"
        break
      fi
    done
  fi

  if [ -z "$selected" ] && lane_exists "$DEFAULT_LANE"; then
    selected="$DEFAULT_LANE"
  fi
  if [ -z "$selected" ] && lane_exists cpu; then
    selected="cpu"
  fi
  if [ -z "$selected" ]; then
    echo "ERROR: no Gema ML lane is installed under $LANES_DIR" >&2
    exit 66
  fi
  printf '%s\n' "$selected"
}

lane="$(select_lane)"
env_dir="/opt/gema/ml/$lane"
cuda_home="$(cuda_home_for_lane "$lane")"
wheel_libs="$(wheel_library_path)"

case "${1:---exports}" in
  --lane)
    printf '%s\n' "$lane"
    ;;
  --env)
    printf '%s\n' "$env_dir"
    ;;
  --cuda-home)
    printf '%s\n' "$cuda_home"
    ;;
  --exports)
    printf 'GEMA_CUDA_LANE_SELECTED=%s\n' "$lane"
    printf 'VIRTUAL_ENV=%s\n' "$env_dir"
    if [ -n "$cuda_home" ] && [ -d "$ROOT$cuda_home" ]; then
      printf 'CUDA_HOME=%s\n' "$cuda_home"
      printf 'CUDA_PATH=%s\n' "$cuda_home"
      printf 'PATH=%s/bin:/opt/gema/bin:%s/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n' "$cuda_home" "$env_dir"
      if [ -n "$wheel_libs" ]; then
        printf 'LD_LIBRARY_PATH=%s/lib64:%s/extras/CUPTI/lib64:%s:${LD_LIBRARY_PATH:-}\n' "$cuda_home" "$cuda_home" "$wheel_libs"
      else
        printf 'LD_LIBRARY_PATH=%s/lib64:%s/extras/CUPTI/lib64:${LD_LIBRARY_PATH:-}\n' "$cuda_home" "$cuda_home"
      fi
    else
      printf 'PATH=/opt/gema/bin:%s/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n' "$env_dir"
      if [ -n "$wheel_libs" ]; then
        printf 'LD_LIBRARY_PATH=%s:${LD_LIBRARY_PATH:-}\n' "$wheel_libs"
      fi
    fi
    ;;
  *)
    echo "usage: gema-cuda-select [--lane|--env|--cuda-home|--exports]" >&2
    exit 64
    ;;
esac
