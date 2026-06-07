#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="${1:-$SCRIPT_DIR/out}"
mkdir -p "$OUT/lib" "$OUT/bin"

cc="${CC:-gcc}"
"$cc" -shared -fPIC -O2 -Wall -Wextra \
  -Wl,-soname,libcuda.so.1 \
  "$SCRIPT_DIR/gema_cuda_proxy_client.c" \
  -o "$OUT/lib/libcuda.so.1"
ln -sf libcuda.so.1 "$OUT/lib/libcuda.so"
install -m 0755 "$SCRIPT_DIR/gema-cuda-proxy-daemon.py" "$OUT/bin/gema-cuda-proxy-daemon"
printf '%s\n' "$OUT"
