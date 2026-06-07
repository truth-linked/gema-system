#!/usr/bin/env bash
set -euo pipefail

ROOT="${GEMA_SYSTEM_ROOT:-/var/lib/gema-system}"
WORKSPACES="$ROOT/workspaces"
DRY_RUN=0
DELETE_EMPTY=0

usage() {
  cat >&2 <<'EOF'
usage: gema-workspace-mount-sweep.sh [--dry-run] [--delete-empty]

Find unhealthy rclone/FUSE workspace mounts under /var/lib/gema-system/workspaces
and detach only the broken mounts. Healthy mounts are left untouched.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --dry-run)
      DRY_RUN=1
      ;;
    --delete-empty)
      DELETE_EMPTY=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage
      exit 64
      ;;
  esac
  shift
done

is_workspace_mountpoint() {
  local path="$1"
  case "$path" in
    "$WORKSPACES"/*/workspace) return 0 ;;
    *) return 1 ;;
  esac
}

is_rclone_fstype() {
  case "$1" in
    fuse.rclone|rclone) return 0 ;;
    *) return 1 ;;
  esac
}

mount_healthy() {
  local path="$1"
  timeout 3 ls "$path" >/dev/null 2>&1
}

detach_mount() {
  local path="$1"
  if [ "$DRY_RUN" = "1" ]; then
    return 0
  fi
  fusermount3 -uz "$path" 2>/dev/null \
    || fusermount -uz "$path" 2>/dev/null \
    || umount -l "$path" 2>/dev/null \
    || return 1
}

delete_empty_workspace_dir() {
  local path="$1"
  [ "$DELETE_EMPTY" = "1" ] || return 0
  [ "$DRY_RUN" = "0" ] || return 0
  rmdir "$path" "$(dirname "$path")" 2>/dev/null || true
}

scanned=0
healthy=0
broken=0
detached=0
failed=0

while read -r target source fstype _rest; do
  is_workspace_mountpoint "$target" || continue
  is_rclone_fstype "$fstype" || continue
  scanned=$((scanned + 1))
  if mount_healthy "$target"; then
    healthy=$((healthy + 1))
    printf 'healthy target=%s source=%s fstype=%s\n' "$target" "$source" "$fstype"
    continue
  fi

  broken=$((broken + 1))
  if [ "$DRY_RUN" = "1" ]; then
    printf 'would-detach target=%s source=%s fstype=%s\n' "$target" "$source" "$fstype"
    continue
  fi
  if detach_mount "$target"; then
    detached=$((detached + 1))
    printf 'detached target=%s source=%s fstype=%s\n' "$target" "$source" "$fstype"
    delete_empty_workspace_dir "$target"
  else
    failed=$((failed + 1))
    printf 'failed target=%s source=%s fstype=%s\n' "$target" "$source" "$fstype" >&2
  fi
done < <(findmnt -rn | awk '{print $1, $2, $3}')

printf 'summary scanned=%s healthy=%s broken=%s detached=%s failed=%s dry_run=%s\n' \
  "$scanned" "$healthy" "$broken" "$detached" "$failed" "$DRY_RUN"

[ "$failed" -eq 0 ]
