#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODE="${1:-free}"
GPU_VENDOR="${2:-}"
REQUIRE_R2="${3:-false}"

exec "$SCRIPT_DIR/gema-hostd.sh" host-probe "$MODE" "$GPU_VENDOR" "$REQUIRE_R2"
