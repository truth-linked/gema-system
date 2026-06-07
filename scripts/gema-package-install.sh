#!/usr/bin/env bash
set -euo pipefail

ROOTFS=""
SESSION_ID=""
MANAGER=""
PACKAGES=()

usage() {
  cat >&2 <<'EOF'
usage:
  gema-package-install.sh --rootfs <path> --session <id> <apt|python|pip|npm|node> <packages...>
  gema-package-install.sh --rootfs <path> --session <id> list
  gema-package-install.sh --rootfs <path> --session <id> verify-lock [latest|lock.json]
  gema-package-install.sh --rootfs <path> --session <id> replay-lock [latest|lock.json]
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --rootfs)
      ROOTFS="${2:-}"
      shift 2
      ;;
    --session)
      SESSION_ID="${2:-}"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    -*)
      echo "ERROR: unknown option: $1" >&2
      exit 64
      ;;
    *)
      MANAGER="$1"
      shift
      PACKAGES=("$@")
      break
      ;;
  esac
done

[ -n "$ROOTFS" ] || { usage; exit 64; }
[ -n "$SESSION_ID" ] || { usage; exit 64; }
[ -n "$MANAGER" ] || { usage; exit 64; }
[ -d "$ROOTFS" ] || { echo "ERROR: rootfs missing: $ROOTFS" >&2; exit 66; }

LOG_DIR="${GEMA_PACKAGE_LOG_DIR:-/var/lib/gema-system/sessions/$SESSION_ID/package-installs}"
MANIFEST="$LOG_DIR/packages.jsonl"
LOCK_DIR="$LOG_DIR/locks"
LATEST_LOCK="$LOG_DIR/latest-lock.json"
APT_ALLOWLIST="${GEMA_APT_ALLOWLIST:-build-essential,curl,git,git-lfs,jq,ffmpeg,tmux,htop,nano,vim,wget,unzip,zip,rsync,openssh-client,ca-certificates,python3,python3-full,python3-pip,python3-venv,nodejs,npm,pkg-config,cmake,make,gcc,g++,libgl1,libglib2.0-0,libsm6,libxext6,libxrender1}"
APT_DENY_REGEX="${GEMA_APT_DENY_REGEX:-^(linux-|systemd|udev|dbus|openssh-server|docker|podman|containerd|runc|iptables|nftables|kmod|module-init-tools|sudo|policykit|polkit|snapd|lxd|ufw|firewalld)}"
PY_DENY_REGEX="${GEMA_PYTHON_DENY_REGEX:-(^-|^\\.|^~|://|@|/|\\.\\.|;|&&|\\|)}"
NPM_DENY_REGEX="${GEMA_NPM_DENY_REGEX:-(^-|^\\.|^~|://|/|\\.\\.|;|&&|\\|)}"
PACKAGE_TIMEOUT_SECONDS="${GEMA_PACKAGE_TIMEOUT_SECONDS:-900}"
APT_CONNECT_TIMEOUT_SECONDS="${GEMA_APT_CONNECT_TIMEOUT_SECONDS:-20}"
APT_RETRIES="${GEMA_APT_RETRIES:-1}"
PYTHON_ENV="${GEMA_PYTHON_ENV:-/opt/gema/ml/cu121}"
export GEMA_PYTHON_ENV="$PYTHON_ENV"
PIP_ONLY_BINARY="${GEMA_PIP_ONLY_BINARY:-:all:}"
NPM_IGNORE_SCRIPTS="${GEMA_NPM_IGNORE_SCRIPTS:-1}"

mkdir -p "$LOG_DIR" "$LOCK_DIR"

json_array() {
  python3 - "$@" <<'PY'
import json, sys
print(json.dumps(sys.argv[1:]))
PY
}

emit_manifest() {
  local status="$1"
  local detail="$2"
  local lock_path="${3:-}"
  python3 - "$MANIFEST" "$SESSION_ID" "$MANAGER" "$status" "$detail" "$lock_path" "${PACKAGES[@]}" <<'PY'
import json, os, sys, time
path, session, manager, status, detail, lock_path, *packages = sys.argv[1:]
record = {
    "session_id": session,
    "manager": manager,
    "packages": packages,
    "status": status,
    "detail": detail,
    "lock_path": lock_path or None,
    "recorded_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
}
with open(path, "a", encoding="utf-8") as fh:
    fh.write(json.dumps(record, separators=(",", ":")) + "\n")
print(json.dumps(record))
PY
}

sha256_file() {
  local path="$1"
  if [ -f "$path" ]; then
    sha256sum "$path" | awk '{print $1}'
  fi
}

collect_package_state() {
  local apt_file="$1"
  local py_file="$2"
  local npm_file="$3"
  chroot_run dpkg-query -W -f='${Package}\t${Version}\t${Architecture}\n' >"$apt_file" 2>/dev/null || : >"$apt_file"
  if [ -x "$ROOTFS$PYTHON_ENV/bin/python" ]; then
    chroot_run "$PYTHON_ENV/bin/python" -m pip freeze --all --local >"$py_file" 2>/dev/null || : >"$py_file"
  else
    : >"$py_file"
  fi
  if [ -x "$ROOTFS/usr/bin/npm" ]; then
    chroot_run npm list -g --depth=0 --json >"$npm_file" 2>/dev/null || printf '{}\n' >"$npm_file"
  else
    printf '{}\n' >"$npm_file"
  fi
}

write_package_lock() {
  local manager="$1"
  local stamp
  local lock_path
  local apt_file
  local py_file
  local npm_file
  stamp="$(date -u +%Y%m%dT%H%M%SZ)"
  lock_path="$LOCK_DIR/$stamp-$manager.json"
  apt_file="$LOCK_DIR/$stamp-apt-installed.tsv"
  py_file="$LOCK_DIR/$stamp-python-freeze.txt"
  npm_file="$LOCK_DIR/$stamp-npm-global.json"

  collect_package_state "$apt_file" "$py_file" "$npm_file"

  python3 - "$lock_path" "$SESSION_ID" "$manager" "$ROOTFS" "$PYTHON_ENV" "$apt_file" "$(sha256_file "$apt_file")" "$py_file" "$(sha256_file "$py_file")" "$npm_file" "$(sha256_file "$npm_file")" "${PACKAGES[@]}" <<'PY'
import json, os, sys, time
(
    lock_path,
    session,
    manager,
    rootfs,
    python_env,
    apt_file,
    apt_sha,
    py_file,
    py_sha,
    npm_file,
    npm_sha,
    *packages,
) = sys.argv[1:]

apt = []
if os.path.exists(apt_file):
    with open(apt_file, encoding="utf-8", errors="ignore") as fh:
        for line in fh:
            parts = line.rstrip("\n").split("\t")
            if len(parts) == 3:
                apt.append({"name": parts[0], "version": parts[1], "arch": parts[2]})

python_freeze = []
if os.path.exists(py_file):
    with open(py_file, encoding="utf-8", errors="ignore") as fh:
        python_freeze = [line.strip() for line in fh if line.strip()]

npm = {}
if os.path.exists(npm_file):
    try:
        with open(npm_file, encoding="utf-8", errors="ignore") as fh:
            npm = json.load(fh)
    except Exception:
        npm = {}

lock = {
    "schema": "gema.package_lock.v1",
    "session_id": session,
    "manager": manager,
    "requested": packages,
    "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "rootfs": rootfs,
    "python_env": python_env,
    "artifacts": {
        "apt_installed": {"path": apt_file, "sha256": apt_sha or None, "count": len(apt)},
        "python_freeze": {"path": py_file, "sha256": py_sha or None, "count": len(python_freeze)},
        "npm_global": {"path": npm_file, "sha256": npm_sha or None},
    },
    "apt": apt,
    "python": {"freeze": python_freeze},
    "npm": npm,
}
with open(lock_path, "w", encoding="utf-8") as fh:
    json.dump(lock, fh, separators=(",", ":"))
    fh.write("\n")
print(lock_path)
PY
  cp "$lock_path" "$LATEST_LOCK"
  printf '%s\n' "$lock_path"
}

resolve_lock_path() {
  local requested="${1:-latest}"
  python3 - "$LOG_DIR" "$LOCK_DIR" "$LATEST_LOCK" "$requested" <<'PY'
import os, sys
log_dir, lock_dir, latest, requested = sys.argv[1:]
if requested in ("", "latest"):
    candidate = latest
elif os.path.isabs(requested):
    candidate = requested
else:
    candidate = os.path.join(lock_dir, requested)
real_log = os.path.realpath(log_dir)
real_candidate = os.path.realpath(candidate)
if real_candidate != real_log and not real_candidate.startswith(real_log + os.sep):
    print(f"ERROR: lock path escapes package install directory: {requested}", file=sys.stderr)
    raise SystemExit(65)
if not os.path.exists(real_candidate):
    print(f"ERROR: package lock not found: {requested}", file=sys.stderr)
    raise SystemExit(66)
print(real_candidate)
PY
}

verify_package_lock() {
  local requested="${1:-latest}"
  local lock_path
  local tmp_dir
  local report_path
  lock_path="$(resolve_lock_path "$requested")"
  tmp_dir="$(mktemp -d "$LOCK_DIR/verify.XXXXXX")"
  report_path="$LOG_DIR/last-verify.json"
  collect_package_state "$tmp_dir/apt-installed.tsv" "$tmp_dir/python-freeze.txt" "$tmp_dir/npm-global.json"
  set +e
  python3 - "$lock_path" "$tmp_dir/apt-installed.tsv" "$tmp_dir/python-freeze.txt" "$tmp_dir/npm-global.json" "$report_path" <<'PY'
import json, os, sys, time
lock_path, apt_file, py_file, npm_file, report_path = sys.argv[1:]
with open(lock_path, encoding="utf-8") as fh:
    lock = json.load(fh)
if lock.get("schema") != "gema.package_lock.v1":
    raise SystemExit("ERROR: unsupported package lock schema")

def read_apt(path):
    rows = {}
    if os.path.exists(path):
        with open(path, encoding="utf-8", errors="ignore") as fh:
            for line in fh:
                parts = line.rstrip("\n").split("\t")
                if len(parts) == 3:
                    rows[parts[0]] = {"version": parts[1], "arch": parts[2]}
    return rows

def read_freeze(path):
    if not os.path.exists(path):
        return []
    with open(path, encoding="utf-8", errors="ignore") as fh:
        return sorted(line.strip() for line in fh if line.strip())

def read_npm(path):
    try:
        with open(path, encoding="utf-8", errors="ignore") as fh:
            data = json.load(fh)
    except Exception:
        data = {}
    deps = data.get("dependencies") if isinstance(data, dict) else {}
    if not isinstance(deps, dict):
        deps = {}
    return {name: (meta or {}).get("version") for name, meta in deps.items()}

expected_apt = {row["name"]: {"version": row.get("version"), "arch": row.get("arch")} for row in lock.get("apt", []) if row.get("name")}
actual_apt = read_apt(apt_file)
expected_py = sorted(lock.get("python", {}).get("freeze", []))
actual_py = read_freeze(py_file)
expected_npm = read_npm(lock.get("artifacts", {}).get("npm_global", {}).get("path", ""))
if not expected_npm:
    expected_npm = {name: (meta or {}).get("version") for name, meta in lock.get("npm", {}).get("dependencies", {}).items()}
actual_npm = read_npm(npm_file)

mismatches = []
if expected_apt != actual_apt:
    mismatches.append({
        "area": "apt",
        "missing": sorted(set(expected_apt) - set(actual_apt)),
        "extra": sorted(set(actual_apt) - set(expected_apt)),
        "changed": sorted(name for name in set(expected_apt) & set(actual_apt) if expected_apt[name] != actual_apt[name]),
    })
if expected_py != actual_py:
    mismatches.append({
        "area": "python",
        "missing": sorted(set(expected_py) - set(actual_py)),
        "extra": sorted(set(actual_py) - set(expected_py)),
    })
if expected_npm != actual_npm:
    mismatches.append({
        "area": "npm",
        "missing": sorted(set(expected_npm) - set(actual_npm)),
        "extra": sorted(set(actual_npm) - set(expected_npm)),
        "changed": sorted(name for name in set(expected_npm) & set(actual_npm) if expected_npm[name] != actual_npm[name]),
    })

report = {
    "ok": not mismatches,
    "schema": "gema.package_verify.v1",
    "lock_path": lock_path,
    "checked_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "mismatches": mismatches,
}
with open(report_path, "w", encoding="utf-8") as fh:
    json.dump(report, fh, separators=(",", ":"))
    fh.write("\n")
print(json.dumps(report, separators=(",", ":")))
raise SystemExit(0 if not mismatches else 2)
PY
  local status=$?
  set -e
  rm -rf "$tmp_dir"
  return "$status"
}

converge_package_lock() {
  local lock_path="$1"
  local tmp_dir
  tmp_dir="$(mktemp -d "$LOCK_DIR/converge.XXXXXX")"
  collect_package_state "$tmp_dir/apt-installed.tsv" "$tmp_dir/python-freeze.txt" "$tmp_dir/npm-global.json"

  mapfile -t apt_extras < <(python3 - "$lock_path" "$tmp_dir/apt-installed.tsv" <<'PY'
import json, sys
lock_path, apt_file = sys.argv[1:]
with open(lock_path, encoding="utf-8") as fh:
    lock = json.load(fh)
expected = {row.get("name") for row in lock.get("apt", []) if row.get("name")}
actual = set()
with open(apt_file, encoding="utf-8", errors="ignore") as fh:
    for line in fh:
        parts = line.rstrip("\n").split("\t")
        if len(parts) == 3:
            actual.add(parts[0])
for name in sorted(actual - expected):
    print(name)
PY
)
  if [ "${#apt_extras[@]}" -gt 0 ]; then
    mapfile -t apt_purge < <(printf '%s\n' "${apt_extras[@]}" | while read -r pkg; do
      [ -n "$pkg" ] || continue
      essential="$(chroot_run dpkg-query -W -f='${Essential}' "$pkg" 2>/dev/null || true)"
      if [ "$essential" != "yes" ]; then
        printf '%s\n' "$pkg"
      fi
    done)
    if [ "${#apt_purge[@]}" -gt 0 ]; then
      chroot_run apt-get purge -y "${apt_purge[@]}"
      chroot_run apt-get autoremove -y --purge
    fi
  fi

  if [ -x "$ROOTFS$PYTHON_ENV/bin/python" ]; then
    mapfile -t py_extras < <(python3 - "$lock_path" "$tmp_dir/python-freeze.txt" <<'PY'
import json, re, sys
lock_path, freeze_file = sys.argv[1:]
with open(lock_path, encoding="utf-8") as fh:
    lock = json.load(fh)
def name(line):
    match = re.match(r"^([A-Za-z0-9_.-]+)==", line)
    return match.group(1).lower().replace("_", "-") if match else None
expected = {name(line) for line in lock.get("python", {}).get("freeze", [])}
expected.discard(None)
actual = set()
with open(freeze_file, encoding="utf-8", errors="ignore") as fh:
    for line in fh:
        parsed = name(line.strip())
        if parsed:
            actual.add(parsed)
for pkg in sorted(actual - expected):
    print(pkg)
PY
)
    if [ "${#py_extras[@]}" -gt 0 ]; then
      chroot_run "$PYTHON_ENV/bin/python" -m pip uninstall -y "${py_extras[@]}"
    fi
  fi

  if [ -x "$ROOTFS/usr/bin/npm" ]; then
    mapfile -t npm_extras < <(python3 - "$lock_path" "$tmp_dir/npm-global.json" <<'PY'
import json, sys
lock_path, npm_file = sys.argv[1:]
with open(lock_path, encoding="utf-8") as fh:
    lock = json.load(fh)
expected = set((lock.get("npm", {}).get("dependencies") or {}).keys())
try:
    with open(npm_file, encoding="utf-8", errors="ignore") as fh:
        current = json.load(fh)
except Exception:
    current = {}
actual = set((current.get("dependencies") or {}).keys())
for pkg in sorted(actual - expected):
    print(pkg)
PY
)
    if [ "${#npm_extras[@]}" -gt 0 ]; then
      chroot_run npm uninstall -g "${npm_extras[@]}"
    fi
  fi

  rm -rf "$tmp_dir"
}

replay_package_lock() {
  local requested="${1:-latest}"
  local lock_path
  local manager
  lock_path="$(resolve_lock_path "$requested")"
  manager="$(python3 - "$lock_path" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as fh:
    print(json.load(fh).get("manager", ""))
PY
)"
  case "$manager" in
    apt)
      mapfile -t PACKAGES < <(python3 - "$lock_path" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as fh:
    lock = json.load(fh)
versions = {row.get("name"): row.get("version") for row in lock.get("apt", []) if row.get("name") and row.get("version")}
for name in lock.get("requested", []):
    if name in versions:
        print(f"{name}={versions[name]}")
PY
)
      [ "${#PACKAGES[@]}" -gt 0 ] || { echo "ERROR: apt lock has no replayable requested packages" >&2; exit 65; }
      for pkg in "${PACKAGES[@]}"; do validate_pkg_name apt "${pkg%%=*}"; done
      ensure_apt_sources
      ensure_policy_rc_d
      apt_get update
      apt_get install -y --no-install-recommends "${PACKAGES[@]}"
      ;;
    python)
      ensure_python_env
      local req_file
      req_file="$ROOTFS/tmp/gema-replay-python-requirements.txt"
      mkdir -p "$ROOTFS/tmp"
      python3 - "$lock_path" "$req_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as fh:
    lock = json.load(fh)
with open(sys.argv[2], "w", encoding="utf-8") as fh:
    for line in lock.get("python", {}).get("freeze", []):
        fh.write(line + "\n")
PY
      chroot_run "$PYTHON_ENV/bin/python" -m pip install --disable-pip-version-check --no-cache-dir --only-binary=:all: -r /tmp/gema-replay-python-requirements.txt
      ;;
    npm)
      ensure_node_npm
      mapfile -t PACKAGES < <(python3 - "$lock_path" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as fh:
    lock = json.load(fh)
deps = lock.get("npm", {}).get("dependencies", {})
for name, meta in sorted(deps.items()):
    version = (meta or {}).get("version")
    print(f"{name}@{version}" if version else name)
PY
)
      [ "${#PACKAGES[@]}" -gt 0 ] || { echo "ERROR: npm lock has no replayable packages" >&2; exit 65; }
      install_npm
      ;;
    *)
      echo "ERROR: unsupported lock manager: $manager" >&2
      exit 65
      ;;
  esac
  converge_package_lock "$lock_path"
  verify_package_lock "$lock_path"
}

validate_pkg_name() {
  local manager="$1"
  local pkg="$2"
  case "$pkg" in
    ""|*[$'\n\r\t ']*)
      echo "ERROR: invalid package name" >&2
      exit 64
      ;;
  esac
  case "$manager" in
    apt)
      if ! printf '%s' "$pkg" | grep -Eq '^[A-Za-z0-9][A-Za-z0-9+._:-]*$'; then
        echo "ERROR: invalid apt package: $pkg" >&2
        exit 64
      fi
      if printf '%s' "$pkg" | grep -Eq "$APT_DENY_REGEX"; then
        echo "ERROR: apt package is blocked by Gema policy: $pkg" >&2
        exit 65
      fi
      if [ "${GEMA_APT_ALLOW_ALL:-0}" != "1" ]; then
        if ! python3 - "$APT_ALLOWLIST" "$pkg" <<'PY'
import sys
allowed = {p.strip() for p in sys.argv[1].split(",") if p.strip()}
raise SystemExit(0 if sys.argv[2] in allowed else 1)
PY
        then
          echo "ERROR: apt package is not in the Gema allowlist: $pkg" >&2
          exit 65
        fi
      fi
      ;;
    python|pip)
      if printf '%s' "$pkg" | grep -Eq "$PY_DENY_REGEX"; then
        echo "ERROR: python package rejected by Gema policy: $pkg" >&2
        exit 65
      fi
      ;;
    npm|node)
      if printf '%s' "$pkg" | grep -Eq "$NPM_DENY_REGEX"; then
        echo "ERROR: npm package rejected by Gema policy: $pkg" >&2
        exit 65
      fi
      ;;
  esac
}

bind_mount_if_needed() {
  local src="$1"
  local dst="$2"
  local type="${3:-bind}"
  mkdir -p "$dst"
  if findmnt -rn -M "$dst" >/dev/null 2>&1; then
    return 0
  fi
  case "$type" in
    bind) mount --bind "$src" "$dst" ;;
    proc) mount -t proc proc "$dst" ;;
    sysfs) mount -t sysfs sysfs "$dst" ;;
    devpts) mount -t devpts devpts "$dst" ;;
    *) echo "ERROR: unknown mount type: $type" >&2; exit 64 ;;
  esac
}

copy_resolv_conf() {
  if [ -f /etc/resolv.conf ]; then
    mkdir -p "$ROOTFS/etc"
    cp /etc/resolv.conf "$ROOTFS/etc/resolv.conf"
  fi
}

unmount_chroot_mounts() {
  local path
  for path in "$ROOTFS/dev/pts" "$ROOTFS/proc" "$ROOTFS/sys" "$ROOTFS/dev"; do
    if findmnt -rn -M "$path" >/dev/null 2>&1; then
      umount "$path" >/dev/null 2>&1 || umount -l "$path" >/dev/null 2>&1 || true
    fi
  done
}

trap unmount_chroot_mounts EXIT

ensure_apt_sources() {
  mkdir -p "$ROOTFS/etc/apt"
  local sources="$ROOTFS/etc/apt/sources.list"
  if [ "${GEMA_MANAGE_APT_SOURCES:-1}" != "1" ]; then
    return 0
  fi
  cat > "$sources" <<'EOF'
deb http://archive.ubuntu.com/ubuntu noble main universe multiverse restricted
deb http://archive.ubuntu.com/ubuntu noble-updates main universe multiverse restricted
deb http://security.ubuntu.com/ubuntu noble-security main universe multiverse restricted
EOF
}

ensure_policy_rc_d() {
  mkdir -p "$ROOTFS/usr/sbin"
  cat > "$ROOTFS/usr/sbin/policy-rc.d" <<'EOF'
#!/bin/sh
exit 101
EOF
  chmod 0755 "$ROOTFS/usr/sbin/policy-rc.d"
}

with_chroot_mounts() {
  bind_mount_if_needed /dev "$ROOTFS/dev" bind
  bind_mount_if_needed /proc "$ROOTFS/proc" proc
  bind_mount_if_needed /sys "$ROOTFS/sys" sysfs
  mkdir -p "$ROOTFS/dev/pts"
  if ! findmnt -rn -M "$ROOTFS/dev/pts" >/dev/null 2>&1; then
    mount -t devpts devpts "$ROOTFS/dev/pts" || true
  fi
  copy_resolv_conf
}

chroot_run() {
  with_chroot_mounts
  timeout --kill-after=30s "$PACKAGE_TIMEOUT_SECONDS" \
    chroot "$ROOTFS" /usr/bin/env -i \
      HOME=/root \
      DEBIAN_FRONTEND=noninteractive \
      PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
      "$@"
}

apt_get() {
  chroot_run apt-get \
    -o "Acquire::Retries=$APT_RETRIES" \
    -o "Acquire::http::Timeout=$APT_CONNECT_TIMEOUT_SECONDS" \
    -o "Acquire::https::Timeout=$APT_CONNECT_TIMEOUT_SECONDS" \
    -o "Acquire::ftp::Timeout=$APT_CONNECT_TIMEOUT_SECONDS" \
    -o "Dpkg::Lock::Timeout=30" \
    "$@"
}

install_apt() {
  [ "${#PACKAGES[@]}" -gt 0 ] || { echo "ERROR: no apt packages supplied" >&2; exit 64; }
  for pkg in "${PACKAGES[@]}"; do validate_pkg_name apt "$pkg"; done
  ensure_apt_sources
  ensure_policy_rc_d
  apt_get update
  apt_get install -y --no-install-recommends "${PACKAGES[@]}"
}

ensure_python_pip() {
  if [ ! -x "$ROOTFS/usr/bin/python3" ]; then
    PACKAGES=(python3)
    install_apt
  fi
  if [ ! -e "$ROOTFS/usr/lib/python3" ] || ! chroot "$ROOTFS" /usr/bin/env -i PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin python3 -c 'import ensurepip' >/dev/null 2>&1; then
    local old=("${PACKAGES[@]}")
    PACKAGES=(python3-full python3-venv)
    install_apt
    PACKAGES=("${old[@]}")
  fi
}

ensure_python_env() {
  ensure_python_pip
  if [ ! -x "$ROOTFS$PYTHON_ENV/bin/python" ] || [ ! -x "$ROOTFS$PYTHON_ENV/bin/pip" ]; then
    rm -rf "$ROOTFS$PYTHON_ENV"
    chroot_run mkdir -p "$(dirname "$PYTHON_ENV")"
    chroot_run python3 -m venv "$PYTHON_ENV"
  fi
  if [ ! -x "$ROOTFS$PYTHON_ENV/bin/python" ]; then
    echo "ERROR: Gema Python environment was not created correctly" >&2
    exit 70
  fi
  chroot_run "$PYTHON_ENV/bin/python" -m ensurepip --upgrade
  if [ ! -x "$ROOTFS$PYTHON_ENV/bin/pip" ]; then
    echo "ERROR: Gema Python environment has no pip" >&2
    exit 70
  fi
  chroot_run "$PYTHON_ENV/bin/python" -m pip install --disable-pip-version-check --no-cache-dir --only-binary=:all: --upgrade pip setuptools wheel
  chroot_run mkdir -p /opt/gema/bin
  if [ ! -x "$ROOTFS/opt/gema/bin/gema-cuda-select" ]; then
    rm -f "$ROOTFS/opt/gema/bin/python" "$ROOTFS/opt/gema/bin/python3" "$ROOTFS/opt/gema/bin/pip" "$ROOTFS/opt/gema/bin/pip3"
    cat > "$ROOTFS/opt/gema/bin/python" <<EOF
#!/bin/sh
exec $PYTHON_ENV/bin/python "\$@"
EOF
    cat > "$ROOTFS/opt/gema/bin/python3" <<EOF
#!/bin/sh
exec $PYTHON_ENV/bin/python "\$@"
EOF
    cat > "$ROOTFS/opt/gema/bin/pip" <<EOF
#!/bin/sh
exec $PYTHON_ENV/bin/python -m pip "\$@"
EOF
    cat > "$ROOTFS/opt/gema/bin/pip3" <<EOF
#!/bin/sh
exec $PYTHON_ENV/bin/python -m pip "\$@"
EOF
    chmod 0755 "$ROOTFS/opt/gema/bin/python" "$ROOTFS/opt/gema/bin/python3" "$ROOTFS/opt/gema/bin/pip" "$ROOTFS/opt/gema/bin/pip3"
  fi
  chroot_run chown -R 1000:1000 "$PYTHON_ENV" /opt/gema/bin
}

install_python() {
  [ "${#PACKAGES[@]}" -gt 0 ] || { echo "ERROR: no python packages supplied" >&2; exit 64; }
  for pkg in "${PACKAGES[@]}"; do validate_pkg_name python "$pkg"; done
  ensure_python_env
  local pip_args=(--disable-pip-version-check --no-cache-dir)
  if [ -n "$PIP_ONLY_BINARY" ]; then
    pip_args+=(--only-binary "$PIP_ONLY_BINARY")
  fi
  chroot_run "$PYTHON_ENV/bin/python" -m pip install "${pip_args[@]}" "${PACKAGES[@]}"
}

ensure_node_npm() {
  if [ ! -x "$ROOTFS/usr/bin/node" ] || [ ! -x "$ROOTFS/usr/bin/npm" ]; then
    local old=("${PACKAGES[@]}")
    PACKAGES=(nodejs npm)
    install_apt
    PACKAGES=("${old[@]}")
  fi
}

install_npm() {
  [ "${#PACKAGES[@]}" -gt 0 ] || { echo "ERROR: no npm packages supplied" >&2; exit 64; }
  for pkg in "${PACKAGES[@]}"; do validate_pkg_name npm "$pkg"; done
  ensure_node_npm
  local npm_args=(-g --omit=dev)
  if [ "$NPM_IGNORE_SCRIPTS" = "1" ]; then
    npm_args+=(--ignore-scripts)
  fi
  chroot_run npm install "${npm_args[@]}" "${PACKAGES[@]}"
}

list_packages() {
  python3 - "$ROOTFS" "$MANIFEST" "$LOCK_DIR" "$LATEST_LOCK" <<'PY'
import json, os, subprocess, sys
rootfs, manifest, lock_dir, latest_lock_path = sys.argv[1:]
def run(args):
    try:
        return subprocess.check_output(args, text=True, stderr=subprocess.DEVNULL).strip()
    except Exception:
        return ""
dpkg_status = os.path.join(rootfs, "var/lib/dpkg/status")
apt = []
if os.path.exists(dpkg_status):
    name = version = None
    for line in open(dpkg_status, encoding="utf-8", errors="ignore"):
        line = line.rstrip("\n")
        if line.startswith("Package: "):
            name = line.split(": ", 1)[1]
        elif line.startswith("Version: "):
            version = line.split(": ", 1)[1]
        elif not line and name:
            apt.append({"name": name, "version": version})
            name = version = None
history = []
if os.path.exists(manifest):
    for line in open(manifest, encoding="utf-8", errors="ignore"):
        try:
            history.append(json.loads(line))
        except Exception:
            pass
python_env = os.path.join(rootfs, os.environ.get("GEMA_PYTHON_ENV", "/opt/gema/ml/cu121").lstrip("/"))
locks = []
if os.path.isdir(lock_dir):
    for name in sorted(os.listdir(lock_dir)):
        if name.endswith(".json"):
            path = os.path.join(lock_dir, name)
            try:
                locks.append({"name": name, "path": path, "bytes": os.path.getsize(path)})
            except Exception:
                pass
latest_lock = None
if os.path.exists(latest_lock_path):
    try:
        with open(latest_lock_path, encoding="utf-8", errors="ignore") as fh:
            latest_lock = json.load(fh)
    except Exception:
        latest_lock = None
print(json.dumps({"ok": True, "apt_count": len(apt), "apt_sample": apt[:80], "python_env": {"path": python_env, "exists": os.path.exists(python_env)}, "history": history, "locks": locks, "latest_lock": latest_lock}))
PY
}

case "$MANAGER" in
  list)
    list_packages
    ;;
  verify-lock)
    verify_package_lock "${PACKAGES[0]:-latest}"
    ;;
  replay-lock)
    replay_package_lock "${PACKAGES[0]:-latest}"
    ;;
  apt)
    install_apt
    lock_path="$(write_package_lock apt | tail -n 1)"
    emit_manifest installed "apt packages installed" "$lock_path"
    ;;
  python|pip)
    MANAGER="python"
    install_python
    lock_path="$(write_package_lock python | tail -n 1)"
    emit_manifest installed "python packages installed" "$lock_path"
    ;;
  npm|node)
    MANAGER="npm"
    install_npm
    lock_path="$(write_package_lock npm | tail -n 1)"
    emit_manifest installed "npm packages installed" "$lock_path"
    ;;
  *)
    echo "ERROR: unsupported package manager: $MANAGER" >&2
    exit 64
    ;;
esac
