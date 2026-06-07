use chrono::Local;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

type HResult<T> = Result<T, String>;
static PORT_ALLOCATIONS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn port_allocations_lock() -> &'static Mutex<()> {
    PORT_ALLOCATIONS_LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Clone)]
struct Ctx {
    root: PathBuf,
    run_dir: PathBuf,
    log_dir: PathBuf,
    socket_path: PathBuf,
    pid_file: PathBuf,
    substrate: PathBuf,
    ports_dir: PathBuf,
    images_dir: PathBuf,
    image_cache_dir: PathBuf,
    volumes_dir: PathBuf,
    port_allocations_path: PathBuf,
    port_audit_path: PathBuf,
    image_catalog_path: PathBuf,
    host_probe_path: PathBuf,
    metrics_interval: Duration,
    port_pool_start: u16,
    port_pool_end: u16,
    default_storage_quota_bytes: u64,
    snapshot_retention_count: u64,
    max_log_bytes: u64,
    log_retention_count: u64,
}

pub fn main(args: Vec<String>) -> ExitCode {
    let ctx = Ctx::new();
    match run_cli(&ctx, args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(1)
        }
    }
}

impl Ctx {
    fn new() -> Self {
        let root = PathBuf::from(
            env::var("GEMA_SYSTEM_ROOT").unwrap_or_else(|_| "/var/lib/gema-system".to_string()),
        );
        let run_dir = PathBuf::from(
            env::var("GEMA_HOSTD_RUN_DIR").unwrap_or_else(|_| "/run/gema-system".to_string()),
        );
        let log_dir = PathBuf::from(
            env::var("GEMA_HOSTD_LOG_DIR").unwrap_or_else(|_| format!("{}/logs", root.display())),
        );
        let script_dir = env::var("GEMA_SYSTEM_SCRIPT_DIR").unwrap_or_else(|_| {
            env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(|parent| parent.to_path_buf()))
                .and_then(|path| path.parent().map(|parent| parent.join("scripts")))
                .filter(|path| path.exists())
                .or_else(|| {
                    env::current_exe()
                        .ok()
                        .and_then(|path| path.parent().map(|parent| parent.to_path_buf()))
                        .and_then(|path| path.parent().map(|parent| parent.to_path_buf()))
                        .and_then(|path| path.parent().map(|parent| parent.join("scripts")))
                })
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "/root/gema-system/scripts".to_string())
        });
        let ports_dir = root.join("ports");
        let images_dir = root.join("images");
        let image_cache_dir = images_dir.join("cache");
        let volumes_dir = root.join("volumes");
        Self {
            socket_path: run_dir.join("hostd.sock"),
            pid_file: run_dir.join("hostd.pid"),
            substrate: PathBuf::from(script_dir).join("gema-substrate-run.sh"),
            port_allocations_path: ports_dir.join("allocations.json"),
            port_audit_path: ports_dir.join("audit.jsonl"),
            image_catalog_path: images_dir.join("catalog.json"),
            host_probe_path: root.join("host_probe.latest.json"),
            metrics_interval: Duration::from_secs_f64(env_f64(
                "GEMA_METRICS_INTERVAL_SECONDS",
                10.0,
            )),
            port_pool_start: env_u16("GEMA_PORT_POOL_START", 20000),
            port_pool_end: env_u16("GEMA_PORT_POOL_END", 29999),
            default_storage_quota_bytes: env_u64(
                "GEMA_STORAGE_QUOTA_BYTES",
                30 * 1024 * 1024 * 1024,
            ),
            snapshot_retention_count: env_u64("GEMA_SNAPSHOT_RETENTION_COUNT", 2),
            max_log_bytes: env_u64("GEMA_MAX_LOG_BYTES", 25 * 1024 * 1024),
            log_retention_count: env_u64("GEMA_LOG_RETENTION_COUNT", 5),
            root,
            run_dir,
            log_dir,
            ports_dir,
            images_dir,
            image_cache_dir,
            volumes_dir,
        }
    }

    fn setup(&self) -> HResult<()> {
        for path in [
            &self.run_dir,
            &self.log_dir,
            &self.root.join("sessions"),
            &self.root.join("workspaces"),
            &self.root.join("runtime-rootfs"),
            &self.ports_dir,
            &self.images_dir,
            &self.image_cache_dir,
            &self.volumes_dir,
        ] {
            fs::create_dir_all(path).map_err(|err| format!("create {}: {err}", path.display()))?;
        }
        Ok(())
    }

    fn session_dir(&self, id: &str) -> PathBuf {
        debug_assert!(valid_session_id(id));
        self.root.join("sessions").join(id)
    }

    fn workspace_dir(&self, id: &str) -> PathBuf {
        debug_assert!(valid_session_id(id));
        self.root.join("workspaces").join(id).join("workspace")
    }

    fn snapshots_dir(&self, id: &str) -> PathBuf {
        debug_assert!(valid_session_id(id));
        self.workspace_dir(id).join(".gema").join("snapshots")
    }

    fn runtime_rootfs_dir(&self, id: &str) -> PathBuf {
        debug_assert!(valid_session_id(id));
        self.root.join("runtime-rootfs").join(id)
    }

    fn package_installer(&self) -> PathBuf {
        self.substrate
            .parent()
            .map(|dir| dir.join("gema-package-install.sh"))
            .unwrap_or_else(|| PathBuf::from("/root/gema-system/scripts/gema-package-install.sh"))
    }
}

fn valid_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > 80 || id.contains("..") {
        return false;
    }
    bytes.iter().enumerate().all(|(idx, b)| {
        b.is_ascii_alphanumeric()
            || (*b == b'-' && idx > 0)
            || (*b == b'_' && idx > 0)
            || (*b == b'.' && idx > 0)
    })
}

fn require_session_id(id: &str) -> HResult<()> {
    if valid_session_id(id) {
        Ok(())
    } else {
        Err("invalid session_id: use 1-80 chars of letters, numbers, dash, underscore, or dot; path traversal is rejected".to_string())
    }
}

fn env_f64(name: &str, default: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u16(name: &str, default: u16) -> u16 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn now() -> String {
    Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string()
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn reserved_ports() -> BTreeSet<u16> {
    [
        22, 25, 53, 80, 443, 631, 2375, 2376, 3000, 3478, 5432, 5555, 5900, 5901, 6379, 8080, 8090,
        8091, 8891, 11434, 30120, 47984, 47989, 47990, 47998, 47999, 48000, 48002, 48010, 63090,
    ]
    .into_iter()
    .collect()
}

fn read_text(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn read_i64(path: &Path, default: i64) -> i64 {
    read_text(path).parse().unwrap_or(default)
}

fn read_json(path: &Path, default: Value) -> Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(default)
}

fn write_json_atomic(path: &Path, payload: &Value) -> HResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), Uuid::new_v4()));
    let mut file = File::create(&tmp).map_err(|err| format!("create {}: {err}", tmp.display()))?;
    serde_json::to_writer_pretty(&mut file, payload).map_err(|err| err.to_string())?;
    file.write_all(b"\n").map_err(|err| err.to_string())?;
    file.sync_all().ok();
    fs::rename(&tmp, path)
        .map_err(|err| format!("rename {} -> {}: {err}", tmp.display(), path.display()))?;
    Ok(())
}

fn write_text_atomic(path: &Path, payload: &str) -> HResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), Uuid::new_v4()));
    let mut file = File::create(&tmp).map_err(|err| format!("create {}: {err}", tmp.display()))?;
    file.write_all(payload.as_bytes())
        .map_err(|err| err.to_string())?;
    file.sync_all().ok();
    fs::rename(&tmp, path)
        .map_err(|err| format!("rename {} -> {}: {err}", tmp.display(), path.display()))?;
    Ok(())
}

fn append_jsonl(path: &Path, payload: &Value) -> HResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| err.to_string())?;
    file.write_all(
        serde_json::to_string(payload)
            .map_err(|err| err.to_string())?
            .as_bytes(),
    )
    .map_err(|err| err.to_string())?;
    file.write_all(b"\n").map_err(|err| err.to_string())?;
    file.sync_all().ok();
    Ok(())
}

fn rotate_log_file(path: &Path, max_bytes: u64, keep: u64) -> HResult<bool> {
    if max_bytes == 0 || keep == 0 {
        return Ok(false);
    }
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(false);
    };
    if metadata.len() < max_bytes {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    let oldest = path.with_extension(format!(
        "{}.{}",
        path.extension().and_then(|v| v.to_str()).unwrap_or("log"),
        keep
    ));
    let _ = fs::remove_file(oldest);
    for index in (1..keep).rev() {
        let from = path.with_extension(format!(
            "{}.{}",
            path.extension().and_then(|v| v.to_str()).unwrap_or("log"),
            index
        ));
        let to = path.with_extension(format!(
            "{}.{}",
            path.extension().and_then(|v| v.to_str()).unwrap_or("log"),
            index + 1
        ));
        if from.exists() {
            let _ = fs::rename(from, to);
        }
    }
    let first = path.with_extension(format!(
        "{}.1",
        path.extension().and_then(|v| v.to_str()).unwrap_or("log")
    ));
    fs::rename(path, first).map_err(|err| format!("rotate {}: {err}", path.display()))?;
    Ok(true)
}

fn rotate_session_logs(ctx: &Ctx, session_id: &str) -> HResult<u64> {
    let dir = ctx.session_dir(session_id).join("logs");
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(0);
    };
    let mut rotated = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path.extension().and_then(|v| v.to_str()) == Some("log")
            && rotate_log_file(&path, ctx.max_log_bytes, ctx.log_retention_count)?
        {
            rotated += 1;
        }
    }
    Ok(rotated)
}

fn log(ctx: &Ctx, message: &str) {
    let line = format!("{} {}\n", now(), message);
    fs::create_dir_all(&ctx.log_dir).ok();
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ctx.log_dir.join("hostd.log"))
    {
        let _ = file.write_all(line.as_bytes());
    }
    print!("{line}");
}

fn runtime_events_path(ctx: &Ctx, session_id: &str) -> PathBuf {
    ctx.session_dir(session_id).join("runtime.events.jsonl")
}

fn append_runtime_event(
    ctx: &Ctx,
    session_id: &str,
    event_type: &str,
    details: Value,
    actor: &str,
    severity: &str,
) -> HResult<Value> {
    let event = json!({
        "id": Uuid::new_v4().to_string(),
        "session_id": session_id,
        "type": event_type,
        "actor": actor,
        "severity": severity,
        "details": details,
        "at": now(),
    });
    append_jsonl(&runtime_events_path(ctx, session_id), &event)?;
    log(
        ctx,
        &format!("event session={session_id} type={event_type} actor={actor}"),
    );
    Ok(event)
}

fn lifecycle_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("lifecycle.json")
}

fn lifecycle_events_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("lifecycle.events.jsonl")
}

fn allowed_transition(from: Option<&str>, to: &str) -> bool {
    match from {
        None => matches!(to, "queued" | "recovering"),
        Some("queued") => matches!(to, "preparing" | "failed"),
        Some("preparing") => matches!(to, "starting" | "failed"),
        Some("starting") => matches!(to, "ready" | "running" | "stopping" | "failed"),
        Some("ready") => matches!(to, "running" | "stopping" | "failed"),
        Some("running") => matches!(to, "recovering" | "stopping" | "failed"),
        Some("recovering") => matches!(to, "running" | "stopping" | "failed"),
        Some("stopping") => matches!(to, "stopped" | "failed"),
        Some("stopped") => matches!(to, "queued" | "preparing"),
        Some("failed") => matches!(to, "queued" | "preparing" | "recovering"),
        _ => false,
    }
}

fn transition(
    ctx: &Ctx,
    session_id: &str,
    target: &str,
    reason: &str,
    details: Value,
    strict: bool,
) -> HResult<Value> {
    let states = [
        "queued",
        "preparing",
        "starting",
        "ready",
        "running",
        "recovering",
        "stopping",
        "stopped",
        "failed",
    ];
    if !states.contains(&target) {
        return Err(format!("invalid lifecycle state: {target}"));
    }
    let current = read_json(&lifecycle_path(ctx, session_id), json!({}));
    let from = current.get("state").and_then(Value::as_str);
    if strict && !allowed_transition(from, target) {
        return Err(format!(
            "invalid lifecycle transition: {:?} -> {target}",
            from
        ));
    }
    let at = now();
    let event = json!({"session_id": session_id, "from": from, "to": target, "reason": reason, "details": details, "at": at});
    let mut next = current.as_object().cloned().unwrap_or_default();
    next.insert("session_id".to_string(), json!(session_id));
    next.insert("state".to_string(), json!(target));
    next.insert(
        "previous_state".to_string(),
        from.map_or(Value::Null, |v| json!(v)),
    );
    next.insert("reason".to_string(), json!(reason));
    next.insert("updated_at".to_string(), json!(at));
    if !next.contains_key("created_at") {
        next.insert("created_at".to_string(), json!(at));
    }
    let next_value = Value::Object(next);
    write_json_atomic(&lifecycle_path(ctx, session_id), &next_value)?;
    append_jsonl(&lifecycle_events_path(ctx, session_id), &event)?;
    append_runtime_event(
        ctx,
        session_id,
        "lifecycle_transition",
        json!({"from": from, "to": target, "reason": reason, "details": event["details"].clone()}),
        "system",
        "info",
    )?;
    log(
        ctx,
        &format!(
            "state session={session_id} {:?}->{target} reason={reason}",
            from
        ),
    );
    Ok(next_value)
}

fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .arg("-lc")
        .arg(format!(
            "command -v '{}' >/dev/null 2>&1",
            name.replace('\'', "'\\''")
        ))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn rclone_bin() -> String {
    for candidate in ["/usr/local/bin/rclone", "/usr/bin/rclone", "/bin/rclone"] {
        let path = Path::new(candidate);
        if path.exists() {
            return candidate.to_string();
        }
    }
    "rclone".to_string()
}

fn command_output(argv: &[&str]) -> Value {
    if argv.is_empty() {
        return json!({"ok": false, "status": 127, "stdout": "", "stderr": "empty command"});
    }
    match Command::new(argv[0]).args(&argv[1..]).output() {
        Ok(out) => json!({
            "ok": out.status.success(),
            "status": out.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&out.stdout).trim().to_string(),
            "stderr": String::from_utf8_lossy(&out.stderr).trim().to_string()
        }),
        Err(_) => json!({"ok": false, "status": 127, "stdout": "", "stderr": "command not found"}),
    }
}

fn gate(name: &str, pass: bool, required: bool, details: Value) -> Value {
    json!({"name": name, "status": if pass { "pass" } else { "fail" }, "required": required, "blocking": required && !pass, "details": details})
}

fn normalize_isolation_lane(value: Option<&str>) -> &'static str {
    match value.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "secure" | "gvisor" | "sandbox" | "untrusted" => "secure",
        "gpu" | "gpu-native" | "ml" | "ml-native" => "gpu-native",
        "desktop" | "desktop-native" | "creative" => "desktop-native",
        _ => "standard",
    }
}

fn isolation_policy(req: &Value) -> Value {
    let lane = normalize_isolation_lane(req["isolation_lane"].as_str());
    let profile = req["profile"].as_str().unwrap_or("noble-ml-cu121");
    let gpu_vendor = req["gpu_vendor"].as_str().unwrap_or("cpu");
    let gpu_like = matches!(lane, "gpu-native" | "desktop-native")
        || matches!(gpu_vendor, "nvidia" | "amd")
        || matches!(profile, "ml" | "gpu-ml" | "creative" | "compat")
        || profile.starts_with("noble-ml-");
    let runsc_available = command_exists("runsc");
    json!({
        "lane": lane,
        "profile": profile,
        "gpu_vendor": gpu_vendor,
        "engine": if lane == "secure" { "gvisor-required" } else { "gema-native-linux" },
        "raw_performance": lane != "secure",
        "requires_gvisor": lane == "secure",
        "gvisor_available": runsc_available,
        "requires_gpu_devices": gpu_like,
        "hardening": true,
        "no_new_privs": true,
        "drop_caps": true,
        "seccomp": true,
        "device_policy": true,
        "network_namespace": true,
        "root_user_allowed": false,
        "notes": match lane {
            "secure" => "CPU-only untrusted workload lane. Launch is blocked unless runsc/gVisor is installed and selected.",
            "gpu-native" => "Near-native GPU lane with cgroups, namespaces, seccomp-BPF, device allowlist, no-new-privs, and no root inside workload.",
            "desktop-native" => "Interactive desktop lane with native display/GPU stack and Gema Linux hardening controls.",
            _ => "General native workspace lane with Gema Linux hardening controls.",
        }
    })
}

fn check_isolation_lane(policy: &Value, required: bool) -> Value {
    let lane = policy["lane"].as_str().unwrap_or("standard");
    let pass = if lane == "secure" {
        policy["gvisor_available"].as_bool().unwrap_or(false)
    } else {
        true
    };
    gate("isolation_lane", pass, required, policy.clone())
}

fn parse_kv_file(path: &Path) -> HashMap<String, String> {
    read_text(path)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

fn read_key_values(path: &Path) -> Map<String, Value> {
    let mut out = Map::new();
    for line in read_text(path).lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let Ok(value) = parts[1].parse::<i64>() {
                out.insert(parts[0].to_string(), json!(value));
            } else {
                out.insert(parts[0].to_string(), json!(parts[1]));
            }
        }
    }
    out
}

fn directory_size_bytes(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            total += directory_size_bytes(&p);
        } else {
            total += meta.len();
        }
    }
    total
}

fn file_count(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            total += file_count(&p);
        } else {
            total += 1;
        }
    }
    total
}

fn check_kernel_cgroup(required: bool) -> Value {
    let controllers = read_text(Path::new("/sys/fs/cgroup/cgroup.controllers"));
    let list: Vec<String> = controllers.split_whitespace().map(str::to_string).collect();
    let missing: Vec<&str> = ["cpu", "memory", "pids"]
        .into_iter()
        .filter(|item| !list.iter().any(|v| v == item))
        .collect();
    let details = json!({
        "kernel": std::env::consts::OS,
        "kernel_release": command_output(&["uname", "-r"])["stdout"].clone(),
        "cgroup_v2": Path::new("/sys/fs/cgroup/cgroup.controllers").exists(),
        "controllers": list,
        "user_namespace": Path::new("/proc/self/ns/user").exists(),
        "network_namespace": Path::new("/proc/self/ns/net").exists(),
        "unshare": command_exists("unshare"),
        "ip": command_exists("ip"),
        "chroot": command_exists("chroot"),
        "missing_controllers": missing,
    });
    let pass = details["cgroup_v2"].as_bool().unwrap_or(false)
        && details["missing_controllers"]
            .as_array()
            .is_some_and(|a| a.is_empty())
        && details["unshare"].as_bool().unwrap_or(false)
        && details["ip"].as_bool().unwrap_or(false)
        && details["chroot"].as_bool().unwrap_or(false);
    gate("kernel_cgroup", pass, required, details)
}

fn apparmor_profiles() -> Vec<String> {
    command_output(&["aa-status"])["stdout"]
        .as_str()
        .unwrap_or("")
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.ends_with("profiles are loaded.")
                || trimmed.ends_with("profiles are in enforce mode.")
                || trimmed.ends_with("profiles are in complain mode.")
                || trimmed.ends_with("profiles are in prompt mode.")
                || trimmed.ends_with("profiles are in kill mode.")
                || trimmed.ends_with("profiles are in unconfined mode.")
            {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect()
}

fn check_security_hardening(required: bool) -> Value {
    let profiles = apparmor_profiles();
    let apparmor_loaded = Path::new("/sys/kernel/security/apparmor").exists();
    let default_profile = env::var("GEMA_APPARMOR_PROFILE")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let cgroup2_devices_file_absent = !Path::new("/sys/fs/cgroup/devices.allow").exists()
        && !Path::new("/sys/fs/cgroup/devices.deny").exists();
    let details = json!({
        "setpriv": command_exists("setpriv"),
        "capsh": command_exists("capsh"),
        "bubblewrap": command_exists("bwrap"),
        "bpftool": command_exists("bpftool"),
        "apparmor": {
            "module_loaded": apparmor_loaded,
            "aa_status": command_exists("aa-status"),
            "aa_exec": command_exists("aa-exec"),
            "apparmor_parser": command_exists("apparmor_parser"),
            "requested_profile": default_profile,
            "docker_default_loaded": profiles.iter().any(|p| p == "docker-default"),
        },
        "no_new_privs": "enforced at session entry with setpriv --nnp",
        "capabilities": "guest capability bounding, inheritable, and ambient sets are dropped at session entry",
        "user": "workload enters the rootfs as uid/gid 1000",
        "device_policy": {
            "mode": "cgroup v2 eBPF device allowlist plus minimal static /dev and explicit /dev bind rules",
            "cgroup_v2_devices": if cgroup2_devices_file_absent { "ebpf_device_program" } else { "legacy_files_available" },
            "bpftool_available": command_exists("bpftool"),
            "clang_available": command_exists("clang"),
            "bpffs": Path::new("/sys/fs/bpf").exists()
        }
    });
    let pass = details["setpriv"].as_bool().unwrap_or(false)
        && apparmor_loaded
        && details["apparmor"]["aa_status"].as_bool().unwrap_or(false)
        && details["apparmor"]["apparmor_parser"]
            .as_bool()
            .unwrap_or(false)
        && details["device_policy"]["bpftool_available"]
            .as_bool()
            .unwrap_or(false)
        && details["device_policy"]["clang_available"]
            .as_bool()
            .unwrap_or(false)
        && details["device_policy"]["bpffs"].as_bool().unwrap_or(false);
    gate("security_hardening", pass, required, details)
}

fn check_image_management(required: bool) -> Value {
    let details = json!({
        "local_rootfs_catalog": true,
        "buildah": command_exists("buildah"),
        "podman": command_exists("podman"),
        "docker": command_exists("docker"),
        "skopeo": command_exists("skopeo"),
        "umoci": command_exists("umoci"),
        "rsync": command_exists("rsync"),
        "cosign": command_exists("cosign"),
        "signature_policy": "cosign enforced when GEMA_IMAGE_COSIGN_PUBLIC_KEY is configured",
        "rootfs_promotion": "pulls are promoted into managed rootfs profiles with umoci when available, otherwise buildah mount plus rsync",
        "cache_dir": env::var("GEMA_SYSTEM_ROOT").unwrap_or_else(|_| "/var/lib/gema-system".to_string()) + "/images/cache"
    });
    let pass = details["buildah"].as_bool().unwrap_or(false)
        && details["rsync"].as_bool().unwrap_or(false)
        && (details["podman"].as_bool().unwrap_or(false)
            || details["docker"].as_bool().unwrap_or(false)
            || details["buildah"].as_bool().unwrap_or(false));
    gate("image_management", pass, required, details)
}

fn check_gpu_runtime(required: bool, vendor: &str) -> Value {
    let nvidia_cdi = Path::new("/etc/cdi/nvidia.yaml").exists()
        || Path::new("/var/run/cdi/nvidia.yaml").exists();
    let nvidia_device_nodes: Vec<&str> = ["/dev/nvidiactl", "/dev/nvidia-uvm", "/dev/nvidia0"]
        .into_iter()
        .filter(|p| Path::new(p).exists())
        .collect();
    let details = json!({
        "vendor": vendor,
        "nvidia": {
            "nvidia_smi": command_exists("nvidia-smi"),
            "nvidia_ctk": command_exists("nvidia-ctk"),
            "cdi_spec_present": nvidia_cdi,
            "device_nodes": nvidia_device_nodes
        },
        "amd": {
            "rocm_smi": command_exists("rocm-smi"),
            "kfd": Path::new("/dev/kfd").exists(),
            "dri": Path::new("/dev/dri").exists()
        },
        "mode": "explicit GPU device binds with CDI detection metadata"
    });
    let pass = if vendor == "amd" {
        details["amd"]["rocm_smi"].as_bool().unwrap_or(false)
            && details["amd"]["kfd"].as_bool().unwrap_or(false)
            && details["amd"]["dri"].as_bool().unwrap_or(false)
    } else {
        details["nvidia"]["nvidia_smi"].as_bool().unwrap_or(false)
            && details["nvidia"]["nvidia_ctk"].as_bool().unwrap_or(false)
            && nvidia_cdi
    };
    gate("gpu_runtime", pass, required, details)
}

fn detect_project_quota_support(ctx: &Ctx) -> Value {
    let out = command_output(&["findmnt", "-T", &ctx.root.display().to_string(), "-J"]);
    if !out["ok"].as_bool().unwrap_or(false) {
        return json!({"supported": false, "fstype": null, "options": [], "reason": out["stderr"].clone()});
    }
    let parsed: Value =
        serde_json::from_str(out["stdout"].as_str().unwrap_or("")).unwrap_or(json!({}));
    let fs0 = parsed["filesystems"]
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let fstype = fs0["fstype"].as_str().unwrap_or("").to_string();
    let options: Vec<String> = fs0["options"]
        .as_str()
        .unwrap_or("")
        .split(',')
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .collect();
    let supported = match fstype.as_str() {
        "xfs" => options.iter().any(|v| v == "prjquota" || v == "pquota"),
        "ext4" => options.iter().any(|v| v == "prjquota"),
        _ => false,
    };
    let reason = if supported {
        Value::Null
    } else if fstype == "ext4" {
        json!("ext4 project quota not mounted; using mount-backed workspace filesystems")
    } else {
        json!(format!("unsupported project quota filesystem: {fstype}"))
    };
    json!({"supported": supported, "fstype": fstype, "options": options, "reason": reason})
}

fn rootfs_profiles(ctx: &Ctx) -> Vec<Value> {
    let mut profiles = Vec::new();
    let rootfs_root = ctx.root.join("rootfs");
    let Ok(entries) = fs::read_dir(rootfs_root) else {
        return profiles;
    };
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().to_string();
        let rootfs = entry.path();
        if !rootfs.is_dir() || !rootfs.join(".gema-rootfs-ready").exists() {
            continue;
        }
        let mut profile = synthesize_image_manifest(&id, &rootfs);
        let manifest_path = rootfs.join("etc/gema-image.json");
        let manifest = read_json(&manifest_path, Value::Null);
        if manifest.is_object() {
            if let (Some(dst), Some(src)) = (profile.as_object_mut(), manifest.as_object()) {
                for (k, v) in src {
                    if !v.is_null() {
                        dst.insert(k.clone(), v.clone());
                    }
                }
                dst.insert("rootfs".to_string(), json!(rootfs.display().to_string()));
            }
        } else {
            let _ = write_json_atomic(&manifest_path, &profile);
        }
        profiles.push(profile);
    }
    profiles
}

fn validate_launch_rootfs(rootfs_path: &Path) -> HResult<()> {
    if !rootfs_path.join(".gema-rootfs-ready").exists() {
        return Err(format!("rootfs is not ready: {}", rootfs_path.display()));
    }
    for required in ["/usr/bin/env", "/usr/bin/setpriv", "/bin/bash"] {
        let path = rootfs_path.join(required.trim_start_matches('/'));
        if !path.exists() {
            return Err(format!(
                "rootfs is not launchable: missing {required} in {}",
                rootfs_path.display()
            ));
        }
    }
    Ok(())
}

fn default_rootfs(ctx: &Ctx) -> HResult<PathBuf> {
    let rootfs_root = ctx.root.join("rootfs");
    for profile in [
        "noble-base",
        "noble-ml-cu121",
        "noble-ml-cu124",
        "noble-ml-cu118",
    ] {
        let candidate = rootfs_root.join(profile);
        if candidate.join(".gema-rootfs-ready").exists() {
            return Ok(candidate);
        }
    }
    rootfs_profiles(ctx)
        .into_iter()
        .find_map(|profile| profile["rootfs"].as_str().map(PathBuf::from))
        .ok_or_else(|| "no managed rootfs profiles are ready".to_string())
}

fn rootfs_for_launch(ctx: &Ctx, req: &Value) -> HResult<PathBuf> {
    let rootfs_root = ctx.root.join("rootfs");
    let rootfs_path = if let Some(rootfs) = req["rootfs"].as_str().filter(|v| !v.trim().is_empty())
    {
        PathBuf::from(rootfs)
    } else if let Some(profile) = req["profile"].as_str().filter(|v| !v.trim().is_empty()) {
        let profiles = rootfs_profiles(ctx);
        let selected = profiles.iter().find(|item| {
            item["id"].as_str() == Some(profile) || item["profile"].as_str() == Some(profile)
        });
        let Some(selected) = selected else {
            return Err(format!("rootfs profile not found: {profile}"));
        };
        let Some(rootfs) = selected["rootfs"].as_str() else {
            return Err(format!("rootfs profile has no rootfs path: {profile}"));
        };
        PathBuf::from(rootfs)
    } else {
        default_rootfs(ctx)?
    };
    if !rootfs_path.starts_with(&rootfs_root) {
        return Err(format!(
            "rootfs must be under managed rootfs directory: {}",
            rootfs_root.display()
        ));
    }
    validate_launch_rootfs(&rootfs_path)?;
    Ok(rootfs_path)
}

fn path_status(path: &Path) -> Value {
    let metadata = fs::metadata(path);
    json!({
        "path": path.display().to_string(),
        "exists": metadata.is_ok(),
        "is_dir": metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false),
        "is_file": metadata.as_ref().map(|m| m.is_file()).unwrap_or(false),
        "mode": metadata.as_ref().map(|m| format!("{:o}", m.permissions().mode() & 0o7777)).unwrap_or_default(),
    })
}

fn executable_status(path: &Path) -> Value {
    let metadata = fs::metadata(path);
    let executable = metadata
        .as_ref()
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
        .unwrap_or(false);
    json!({
        "path": path.display().to_string(),
        "exists": metadata.is_ok(),
        "executable": executable,
        "mode": metadata.as_ref().map(|m| format!("{:o}", m.permissions().mode() & 0o7777)).unwrap_or_default(),
    })
}

fn substrate_preflight(
    ctx: &Ctx,
    session_id: &str,
    launch_rootfs: &Path,
    workspace: &Path,
    command: &[Value],
    req: &Value,
    runtime_rootfs: &Value,
) -> HResult<Value> {
    let mut failures = Vec::new();
    if !valid_session_id(session_id) {
        failures.push(format!("invalid session_id: {session_id}"));
    }
    if ctx
        .substrate
        .metadata()
        .map(|m| !m.is_file() || (m.permissions().mode() & 0o111) == 0)
        .unwrap_or(true)
    {
        failures.push(format!(
            "substrate is not executable: {}",
            ctx.substrate.display()
        ));
    }
    if let Err(err) = validate_launch_rootfs(launch_rootfs) {
        failures.push(err);
    }
    if runtime_rootfs["mode"].as_str() != Some("overlay") {
        failures.push("runtime rootfs must be a per-session overlay".to_string());
    }
    if runtime_rootfs["mounted"].as_bool() != Some(true) {
        failures.push("runtime rootfs overlay is not mounted".to_string());
    }
    if runtime_rootfs["rootfs"].as_str() == runtime_rootfs["base_rootfs"].as_str() {
        failures.push("runtime rootfs must not point at the shared base rootfs".to_string());
    }
    if runtime_rootfs["rootfs"]
        .as_str()
        .map(Path::new)
        .is_some_and(|rootfs| rootfs != launch_rootfs)
    {
        failures.push("launch rootfs does not match prepared runtime overlay".to_string());
    }
    if !workspace.is_dir() {
        failures.push(format!(
            "workspace is not mounted as a directory: {}",
            workspace.display()
        ));
    }
    let agent_dir = workspace.join(".gema").join("agent");
    if fs::create_dir_all(&agent_dir).is_err() || !agent_dir.is_dir() {
        failures.push(format!(
            "agent state directory unavailable: {}",
            agent_dir.display()
        ));
    }
    if command.is_empty()
        || command
            .iter()
            .any(|part| part.as_str().unwrap_or("").is_empty())
    {
        failures.push("command must be a non-empty argv array".to_string());
    }
    if command
        .iter()
        .filter_map(Value::as_str)
        .any(|part| part.as_bytes().contains(&0))
    {
        failures.push("command argv contains a NUL byte".to_string());
    }

    let hardening = req["hardening"].as_str().unwrap_or("1") != "0";
    let required_tools = [
        "unshare", "mount", "umount", "findmnt", "ip", "setpriv", "setsid",
    ];
    let mut tools = Map::new();
    for tool in required_tools {
        let ok = command_exists(tool);
        if !ok {
            failures.push(format!("required host tool missing: {tool}"));
        }
        tools.insert(tool.to_string(), json!({"available": ok, "required": true}));
    }
    for tool in ["bpftool", "clang"] {
        let ok = command_exists(tool);
        if hardening && !ok {
            failures.push(format!("hardening requires host tool: {tool}"));
        }
        tools.insert(
            tool.to_string(),
            json!({"available": ok, "required": hardening}),
        );
    }

    let preflight = json!({
        "schema": "gema.substrate_preflight.v1",
        "ok": failures.is_empty(),
        "session_id": session_id,
        "checked_at": now(),
        "substrate": executable_status(&ctx.substrate),
        "rootfs": path_status(launch_rootfs),
        "workspace": path_status(workspace),
        "agent_dir": path_status(&agent_dir),
        "runtime_rootfs": runtime_rootfs,
        "hardening": hardening,
        "tools": tools,
        "command_argv_len": command.len(),
        "failures": failures,
    });
    write_json_atomic(
        &ctx.session_dir(session_id).join("substrate-preflight.json"),
        &preflight,
    )?;
    append_runtime_event(
        ctx,
        session_id,
        if preflight["ok"].as_bool().unwrap_or(false) {
            "substrate_preflight_passed"
        } else {
            "substrate_preflight_failed"
        },
        preflight.clone(),
        "system",
        if preflight["ok"].as_bool().unwrap_or(false) {
            "info"
        } else {
            "error"
        },
    )?;
    if preflight["ok"].as_bool() != Some(true) {
        return Err(format!(
            "substrate preflight failed: {}",
            preflight["failures"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_else(|| "unknown failure".to_string())
        ));
    }
    Ok(preflight)
}

fn check_filesystem(ctx: &Ctx, required: bool) -> Value {
    let paths = [
        ("root", ctx.root.clone()),
        ("sessions", ctx.root.join("sessions")),
        ("workspaces", ctx.root.join("workspaces")),
        ("rootfs", ctx.root.join("rootfs")),
    ];
    let mut writes = Map::new();
    for (name, path) in paths {
        let ok = fs::create_dir_all(&path)
            .and_then(|_| {
                let probe = path.join(format!(".probe-{}-{}", std::process::id(), Uuid::new_v4()));
                fs::write(&probe, b"ok\n")?;
                fs::remove_file(probe)
            })
            .is_ok();
        writes.insert(
            name.to_string(),
            json!({"path": path.display().to_string(), "writable": ok}),
        );
    }
    let disk = command_output(&[
        "df",
        "-B1",
        "--output=size,used,avail",
        &ctx.root.display().to_string(),
    ]);
    let mount_tools = command_exists("mkfs.ext4")
        && command_exists("losetup")
        && command_exists("mount")
        && command_exists("findmnt")
        && command_exists("tune2fs");
    let profiles = rootfs_profiles(ctx);
    let pass = writes
        .values()
        .all(|v| v["writable"].as_bool().unwrap_or(false))
        && !profiles.is_empty()
        && mount_tools;
    gate(
        "filesystem",
        pass,
        required,
        json!({
            "paths": writes,
            "disk": disk,
            "rootfs_profiles": profiles.iter().filter_map(|p| p["id"].as_str()).collect::<Vec<_>>(),
            "mount_backed_workspaces": {"mkfs_ext4": command_exists("mkfs.ext4"), "losetup": command_exists("losetup"), "mount": command_exists("mount"), "findmnt": command_exists("findmnt"), "tune2fs": command_exists("tune2fs")},
            "project_quota": detect_project_quota_support(ctx),
        }),
    )
}

fn check_gpu(required: bool, vendor: &str) -> Value {
    if vendor == "amd" {
        let rocm = if command_exists("rocm-smi") {
            command_output(&["rocm-smi"])
        } else {
            json!({"ok": false, "stderr": "rocm-smi missing"})
        };
        return gate(
            "amd_gpu",
            rocm["ok"].as_bool().unwrap_or(false),
            required,
            json!({"requested_vendor": vendor, "rocm_smi": rocm}),
        );
    }
    let nvidia = if command_exists("nvidia-smi") {
        command_output(&["nvidia-smi"])
    } else {
        json!({"ok": false, "stderr": "nvidia-smi missing"})
    };
    let query = if nvidia["ok"].as_bool().unwrap_or(false) {
        command_output(&[
            "nvidia-smi",
            "--query-gpu=name,driver_version,memory.total,compute_cap",
            "--format=csv,noheader",
        ])
    } else {
        json!({"ok": false, "stdout": "", "stderr": nvidia["stderr"].clone()})
    };
    let devices: Vec<String> = query["stdout"]
        .as_str()
        .unwrap_or("")
        .lines()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect();
    gate(
        "nvidia_gpu",
        nvidia["ok"].as_bool().unwrap_or(false) && !devices.is_empty(),
        required,
        json!({"requested_vendor": vendor, "nvidia_smi": nvidia, "query": query, "devices": devices}),
    )
}

fn check_encoder(required: bool) -> Value {
    let ffmpeg = command_exists("ffmpeg");
    let output = if ffmpeg {
        command_output(&["ffmpeg", "-hide_banner", "-encoders"])
    } else {
        json!({})
    };
    let encoders = format!(
        "{}\n{}",
        output["stdout"].as_str().unwrap_or(""),
        output["stderr"].as_str().unwrap_or("")
    );
    let details = json!({
        "ffmpeg": if ffmpeg { "ffmpeg" } else { "" },
        "ffmpeg_status": output["status"].clone(),
        "encoders": {
            "h264_nvenc": encoders.contains("h264_nvenc"),
            "hevc_nvenc": encoders.contains("hevc_nvenc"),
            "h264_vaapi": encoders.contains("h264_vaapi"),
            "hevc_vaapi": encoders.contains("hevc_vaapi"),
            "h264_amf": encoders.contains("h264_amf"),
            "hevc_amf": encoders.contains("hevc_amf")
        },
        "sunshine": command_exists("sunshine")
    });
    let pass = ffmpeg
        && details["encoders"]
            .as_object()
            .is_some_and(|m| m.values().any(|v| v.as_bool().unwrap_or(false)));
    gate("encoder", pass, required, details)
}

fn port_key(protocol: &str, port: u16) -> String {
    format!("{}/{port}", normalize_protocol(protocol))
}

fn normalize_protocol(protocol: &str) -> String {
    match protocol.to_ascii_lowercase().as_str() {
        "udp" => "udp".to_string(),
        _ => "tcp".to_string(),
    }
}

fn normalize_visibility(visibility: &str) -> String {
    match visibility.to_ascii_lowercase().as_str() {
        "private" => "private".to_string(),
        _ => "public".to_string(),
    }
}

fn load_port_allocations(ctx: &Ctx) -> Value {
    read_json(&ctx.port_allocations_path, json!({"allocations": {}}))
}

fn save_port_allocations(ctx: &Ctx, mut doc: Value) -> HResult<()> {
    doc.as_object_mut().unwrap().insert(
        "pool".to_string(),
        json!({"start": ctx.port_pool_start, "end": ctx.port_pool_end}),
    );
    doc.as_object_mut()
        .unwrap()
        .insert("updated_at".to_string(), json!(now()));
    write_json_atomic(&ctx.port_allocations_path, &doc)
}

fn port_is_bindable(protocol: &str, port: u16) -> bool {
    if normalize_protocol(protocol) == "udp" {
        UdpSocket::bind(("0.0.0.0", port)).is_ok()
    } else {
        TcpListener::bind(("0.0.0.0", port)).is_ok()
    }
}

fn validate_host_port(
    protocol: &str,
    port: u16,
    allocations: &Value,
    session_id: Option<&str>,
) -> Option<String> {
    if reserved_ports().contains(&port) || port < 1024 {
        return Some(format!("reserved platform port: {port}"));
    }
    let key = port_key(protocol, port);
    if let Some(existing) = allocations["allocations"].get(&key) {
        if existing["session_id"].as_str() != session_id {
            return Some(format!(
                "port already allocated to session {}: {}/{}",
                existing["session_id"].as_str().unwrap_or("unknown"),
                normalize_protocol(protocol),
                port
            ));
        }
    }
    if !port_is_bindable(protocol, port) {
        return Some(format!(
            "host port already in use: {}/{}",
            normalize_protocol(protocol),
            port
        ));
    }
    None
}

fn choose_available_port(ctx: &Ctx, protocol: &str, allocations: &Value) -> HResult<u16> {
    for port in ctx.port_pool_start..=ctx.port_pool_end {
        if validate_host_port(protocol, port, allocations, None).is_none() {
            return Ok(port);
        }
    }
    Err(format!(
        "no available {protocol} ports in pool {}-{}",
        ctx.port_pool_start, ctx.port_pool_end
    ))
}

fn append_port_audit(
    ctx: &Ctx,
    event_type: &str,
    details: Value,
    actor: &str,
    severity: &str,
) -> HResult<Value> {
    let event = json!({"id": Uuid::new_v4().to_string(), "type": event_type, "actor": actor, "severity": severity, "details": details, "at": now()});
    append_jsonl(&ctx.port_audit_path, &event)?;
    Ok(event)
}

fn session_port_allocations_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("port_allocations.json")
}

fn write_session_port_allocations(ctx: &Ctx, session_id: &str, allocations: &Value) -> HResult<()> {
    let mut active: Vec<Value> = allocations["allocations"]
        .as_object()
        .map(|m| {
            m.values()
                .filter(|a| a["session_id"].as_str() == Some(session_id))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let old = read_json(
        &session_port_allocations_path(ctx, session_id),
        json!({"history": []}),
    );
    if let Some(arr) = old["active"].as_array() {
        active.extend(
            arr.iter()
                .filter(|a| a["visibility"].as_str() == Some("private"))
                .cloned(),
        );
    }
    write_json_atomic(
        &session_port_allocations_path(ctx, session_id),
        &json!({"session_id": session_id, "active": active, "history": old["history"].clone(), "updated_at": now()}),
    )
}

fn upsert_router_record(ctx: &Ctx, session_id: &str, record: &Value) -> HResult<()> {
    let path = ctx.session_dir(session_id).join("ports.jsonl");
    let mut records: Vec<Value> = read_text(&path)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|item: &Value| {
            !(item["protocol"] == record["protocol"]
                && item["host_port"] == record["host_port"]
                && item["guest_port"] == record["guest_port"])
        })
        .collect();
    records.push(record.clone());
    let mut body = String::new();
    for item in records {
        body.push_str(&serde_json::to_string(&item).map_err(|err| err.to_string())?);
        body.push('\n');
    }
    write_text_atomic(&path, &body)
}

fn stop_existing_router_for_mapping(
    ctx: &Ctx,
    session_id: &str,
    protocol: &str,
    host_port: u16,
    guest_port: u16,
) -> HResult<()> {
    for record in read_ports_jsonl(ctx, session_id) {
        let matches_mapping = record["protocol"].as_str() == Some(protocol)
            && record["host_port"].as_u64() == Some(host_port as u64)
            && record["guest_port"].as_u64() == Some(guest_port as u64);
        if !matches_mapping {
            continue;
        }
        if let Some(pid) = record["pid"].as_i64() {
            if pid_alive(pid) {
                let _ = Command::new("kill").arg(pid.to_string()).status();
            }
        }
    }
    Ok(())
}

fn start_allocated_port_router(
    ctx: &Ctx,
    session_id: &str,
    allocation: &Value,
    actor: &str,
) -> HResult<()> {
    if allocation["visibility"].as_str() != Some("public") {
        return Ok(());
    }
    let protocol = allocation["protocol"].as_str().unwrap_or("tcp").to_string();
    let Some(host_port) = allocation["host_port"].as_u64().and_then(|port| u16::try_from(port).ok()) else {
        return Ok(());
    };
    let Some(guest_port) = allocation["guest_port"].as_u64().and_then(|port| u16::try_from(port).ok()) else {
        return Ok(());
    };
    let network = read_json(&ctx.session_dir(session_id).join("network.json"), json!({}));
    let guest_ip = network["guest_ip"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "session network is not ready for port routing".to_string())?
        .to_string();

    stop_existing_router_for_mapping(ctx, session_id, &protocol, host_port, guest_port)?;
    let log_path = ctx
        .session_dir(session_id)
        .join(format!("logs/router-{host_port}-{guest_port}.log"));
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|err| err.to_string())?;
    let log_err = log.try_clone().map_err(|err| err.to_string())?;
    let child = Command::new(env::current_exe().map_err(|err| err.to_string())?)
        .arg("hostd")
        .arg("router")
        .arg(&protocol)
        .arg(host_port.to_string())
        .arg(&guest_ip)
        .arg(guest_port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|err| format!("start router {protocol}/{host_port}->{guest_ip}:{guest_port}: {err}"))?;
    let pid = child.id() as i64;
    let record = json!({
        "protocol": protocol,
        "host_port": host_port,
        "guest_ip": guest_ip,
        "guest_port": guest_port,
        "pid": pid,
        "router": "gema-runtime",
        "allocated_at": now()
    });
    upsert_router_record(ctx, session_id, &record)?;
    register_process(
        ctx,
        session_id,
        "service",
        pid,
        json!(["gema-runtime", "hostd", "router", record["protocol"].clone(), host_port, record["guest_ip"].clone(), guest_port]),
        "session",
        json!(log_path.display().to_string()),
        "port_router",
        record.clone(),
    )?;
    append_runtime_event(ctx, session_id, "port_router_started", record, actor, "info")?;
    Ok(())
}

fn allocate_port(
    ctx: &Ctx,
    session_id: &str,
    guest_port: u16,
    protocol: &str,
    requested_host_port: Option<Value>,
    visibility: &str,
    actor: &str,
) -> HResult<Value> {
    let _guard = port_allocations_lock()
        .lock()
        .map_err(|_| "port allocation lock poisoned".to_string())?;
    let protocol = normalize_protocol(protocol);
    let visibility = normalize_visibility(visibility);
    let requested = requested_host_port.and_then(|v| {
        if v.is_null() || v == json!("") || v == json!("auto") || v == json!(0) || v == json!("0") {
            None
        } else if let Some(n) = v.as_u64() {
            Some(n as u16)
        } else {
            v.as_str().and_then(|s| s.parse().ok())
        }
    });
    if visibility == "private" {
        let allocation = json!({"id": Uuid::new_v4().to_string(), "session_id": session_id, "protocol": protocol, "visibility": visibility, "host_port": null, "guest_port": guest_port, "requested_host_port": requested, "allocated_at": now()});
        let path = session_port_allocations_path(ctx, session_id);
        let mut doc = read_json(
            &path,
            json!({"session_id": session_id, "active": [], "history": []}),
        );
        let mut active = doc["active"].as_array().cloned().unwrap_or_default();
        active.retain(|item| {
            !(item["protocol"] == protocol
                && item["guest_port"] == guest_port
                && item["visibility"] == "private")
        });
        active.push(allocation.clone());
        doc["active"] = Value::Array(active);
        doc["updated_at"] = json!(now());
        write_json_atomic(&path, &doc)?;
        append_port_audit(ctx, "port_allocated", allocation.clone(), actor, "info")?;
        append_runtime_event(
            ctx,
            session_id,
            "port_allocated",
            allocation.clone(),
            actor,
            "info",
        )?;
        return Ok(allocation);
    }
    let mut allocations = load_port_allocations(ctx);
    let host_port = if let Some(port) = requested {
        port
    } else {
        choose_available_port(ctx, &protocol, &allocations)?
    };
    if let Some(conflict) = validate_host_port(&protocol, host_port, &allocations, Some(session_id))
    {
        let details = json!({"session_id": session_id, "protocol": protocol, "visibility": visibility, "requested_host_port": requested, "guest_port": guest_port, "reason": conflict});
        append_port_audit(
            ctx,
            "port_allocation_failed",
            details.clone(),
            actor,
            "warning",
        )?;
        append_runtime_event(
            ctx,
            session_id,
            "port_allocation_failed",
            details,
            actor,
            "warning",
        )?;
        return Err(conflict);
    }
    let allocation = json!({"id": Uuid::new_v4().to_string(), "session_id": session_id, "protocol": protocol, "visibility": visibility, "host_port": host_port, "guest_port": guest_port, "requested_host_port": requested, "allocated_at": now()});
    allocations["allocations"]
        .as_object_mut()
        .unwrap()
        .insert(port_key(&protocol, host_port), allocation.clone());
    save_port_allocations(ctx, allocations.clone())?;
    write_session_port_allocations(ctx, session_id, &allocations)?;
    append_port_audit(ctx, "port_allocated", allocation.clone(), actor, "info")?;
    append_runtime_event(
        ctx,
        session_id,
        "port_allocated",
        allocation.clone(),
        actor,
        "info",
    )?;
    append_runtime_event(
        ctx,
        session_id,
        "port_mapped",
        allocation.clone(),
        actor,
        "info",
    )?;
    start_allocated_port_router(ctx, session_id, &allocation, actor)?;
    Ok(allocation)
}

fn release_session_ports(ctx: &Ctx, session_id: &str, actor: &str) -> HResult<Value> {
    let _guard = port_allocations_lock()
        .lock()
        .map_err(|_| "port allocation lock poisoned".to_string())?;
    let mut released = Vec::new();
    let mut allocations = load_port_allocations(ctx);
    let keys: Vec<String> = allocations["allocations"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v["session_id"].as_str() == Some(session_id))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();
    for key in keys {
        if let Some(mut item) = allocations["allocations"]
            .as_object_mut()
            .unwrap()
            .remove(&key)
        {
            item["released_at"] = json!(now());
            released.push(item);
        }
    }
    save_port_allocations(ctx, allocations)?;
    let path = session_port_allocations_path(ctx, session_id);
    let mut doc = read_json(
        &path,
        json!({"session_id": session_id, "active": [], "history": []}),
    );
    if let Some(active) = doc["active"].as_array() {
        for item in active {
            if item["visibility"].as_str() == Some("private") {
                let mut item = item.clone();
                item["released_at"] = json!(now());
                released.push(item);
            }
        }
    }
    let mut history = doc["history"].as_array().cloned().unwrap_or_default();
    history.extend(released.iter().cloned());
    doc["active"] = json!([]);
    doc["history"] = Value::Array(history);
    doc["updated_at"] = json!(now());
    write_json_atomic(&path, &doc)?;
    for item in &released {
        if let (Some(protocol), Some(host_port), Some(guest_port)) = (
            item["protocol"].as_str(),
            item["host_port"].as_u64().and_then(|port| u16::try_from(port).ok()),
            item["guest_port"].as_u64().and_then(|port| u16::try_from(port).ok()),
        ) {
            let _ = stop_existing_router_for_mapping(ctx, session_id, protocol, host_port, guest_port);
        }
        append_port_audit(ctx, "port_released", item.clone(), actor, "info")?;
        append_runtime_event(
            ctx,
            session_id,
            "port_released",
            item.clone(),
            actor,
            "info",
        )?;
    }
    Ok(Value::Array(released))
}

fn check_port_range(ctx: &Ctx, required: bool) -> Value {
    let allocations = load_port_allocations(ctx);
    let reserved_overlap: Vec<u16> = reserved_ports()
        .into_iter()
        .filter(|p| *p >= ctx.port_pool_start && *p <= ctx.port_pool_end)
        .collect();
    let mut available = 0;
    for port in ctx.port_pool_start..=ctx.port_pool_end {
        if validate_host_port("tcp", port, &allocations, None).is_none() {
            available += 1;
            if available >= 10 {
                break;
            }
        }
    }
    gate(
        "port_range",
        ctx.port_pool_start >= 1024
            && ctx.port_pool_start <= ctx.port_pool_end
            && reserved_overlap.is_empty()
            && available >= 10,
        required,
        json!({"start": ctx.port_pool_start, "end": ctx.port_pool_end, "valid_range": ctx.port_pool_start >= 1024 && ctx.port_pool_start <= ctx.port_pool_end, "reserved_overlap": reserved_overlap, "available_tcp_sample": available}),
    )
}

fn check_r2_credentials(required: bool) -> Value {
    let remote = env::var("GEMA_R2_REMOTE")
        .or_else(|_| env::var("R2_REMOTE"))
        .unwrap_or_else(|_| "r2:gema-cloud-volumes".to_string());
    if !command_exists("rclone") {
        return gate(
            "r2_credentials",
            false,
            required,
            json!({"remote": remote, "rclone": false, "configured_remotes": [], "error": "rclone missing"}),
        );
    }
    let remotes = command_output(&["rclone", "listremotes"]);
    let configured: Vec<String> = remotes["stdout"]
        .as_str()
        .unwrap_or("")
        .lines()
        .map(|l| l.trim_end_matches(':').to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let remote_name = remote.split(':').next().unwrap_or("");
    if !configured.iter().any(|v| v == remote_name) {
        return gate(
            "r2_credentials",
            false,
            required,
            json!({"remote": remote, "rclone": true, "configured_remotes": configured, "error": format!("remote not configured: {remote_name}")}),
        );
    }
    let probe = command_output(&["rclone", "lsf", &remote, "--max-depth", "1"]);
    gate(
        "r2_credentials",
        probe["ok"].as_bool().unwrap_or(false),
        required,
        json!({"remote": remote, "rclone": true, "configured_remotes": configured, "probe_status": probe["status"].clone(), "probe_error": probe["stderr"].clone()}),
    )
}

fn check_daemon_health(ctx: &Ctx, required: bool) -> Value {
    let pid = read_text(&ctx.pid_file);
    let pid_alive = !pid.is_empty() && Path::new(&format!("/proc/{pid}")).exists();
    let details = json!({"pid_file": ctx.pid_file.display().to_string(), "pid": pid, "pid_alive": pid_alive, "socket": ctx.socket_path.display().to_string(), "socket_exists": ctx.socket_path.exists(), "substrate": ctx.substrate.display().to_string(), "substrate_executable": ctx.substrate.exists()});
    gate(
        "daemon_health",
        pid_alive && ctx.socket_path.exists() && ctx.substrate.exists(),
        required,
        details,
    )
}

fn host_probe(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let mode = req["mode"].as_str().unwrap_or("free");
    let policy = isolation_policy(req);
    let gpu_vendor = req["gpu_vendor"]
        .as_str()
        .unwrap_or(if matches!(mode, "paid-gpu" | "gpu") {
            "nvidia"
        } else {
            "none"
        });
    let paid_gpu = matches!(mode, "paid-gpu" | "gpu") || matches!(gpu_vendor, "nvidia" | "amd");
    let checks = vec![
        check_isolation_lane(&policy, policy["lane"].as_str() == Some("secure")),
        check_kernel_cgroup(true),
        check_security_hardening(true),
        check_image_management(paid_gpu),
        check_filesystem(ctx, true),
        check_gpu(
            paid_gpu,
            if matches!(gpu_vendor, "nvidia" | "amd") {
                gpu_vendor
            } else {
                "nvidia"
            },
        ),
        check_gpu_runtime(
            paid_gpu,
            if matches!(gpu_vendor, "nvidia" | "amd") {
                gpu_vendor
            } else {
                "nvidia"
            },
        ),
        check_encoder(paid_gpu),
        check_port_range(ctx, true),
        check_r2_credentials(paid_gpu || req["require_r2"].as_bool().unwrap_or(false)),
        check_daemon_health(ctx, true),
    ];
    let blocking: Vec<Value> = checks
        .iter()
        .filter(|v| v["blocking"].as_bool().unwrap_or(false))
        .cloned()
        .collect();
    let warnings: Vec<Value> = checks
        .iter()
        .filter(|v| v["status"] == "fail" && !v["blocking"].as_bool().unwrap_or(false))
        .cloned()
        .collect();
    let result = json!({"ok": blocking.is_empty(), "mode": mode, "paid_gpu": paid_gpu, "isolation": policy, "generated_at": now(), "checks": checks, "blocking": blocking, "warnings": warnings});
    write_json_atomic(&ctx.host_probe_path, &result)?;
    Ok(result)
}

fn require_host_probe(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let policy = isolation_policy(req);
    let gpu_vendor = req["gpu_vendor"].as_str().unwrap_or("cpu");
    let paid_gpu = req["paid_gpu"].as_bool().unwrap_or(false)
        || matches!(gpu_vendor, "nvidia" | "amd")
        || matches!(req["mode"].as_str(), Some("paid-gpu" | "gpu"));
    let secure_lane = policy["lane"].as_str() == Some("secure");
    if !paid_gpu && !secure_lane && !req["require_preflight"].as_bool().unwrap_or(false) {
        return Ok(Value::Null);
    }
    let probe = host_probe(
        ctx,
        &json!({"mode": if paid_gpu { "paid-gpu" } else { "free" }, "gpu_vendor": gpu_vendor, "require_r2": req["require_r2"].as_bool().unwrap_or(paid_gpu), "isolation_lane": policy["lane"].clone(), "profile": req["profile"].clone()}),
    )?;
    if !probe["ok"].as_bool().unwrap_or(false) {
        let empty = Vec::new();
        let names = probe["blocking"]
            .as_array()
            .unwrap_or(&empty)
            .iter()
            .filter_map(|v| v["name"].as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!("host probe failed: {names}"));
    }
    Ok(probe)
}

fn launch_readiness_path(ctx: &Ctx, session_id: &str) -> PathBuf {
    ctx.session_dir(session_id).join("launch_readiness.json")
}

fn requested_host_port(req: &Value) -> Option<u16> {
    req.get("host_port").and_then(|v| {
        if v.is_null() || v == "auto" || v == "" || v == 0 || v == "0" {
            None
        } else if let Some(n) = v.as_u64() {
            Some(n as u16)
        } else {
            v.as_str().and_then(|s| s.parse().ok())
        }
    })
}

fn readiness_check(name: &str, status: &str, blocking: bool, details: Value) -> Value {
    json!({"name": name, "status": status, "blocking": blocking, "details": details})
}

fn build_launch_readiness(ctx: &Ctx, req: &Value, write: bool) -> HResult<Value> {
    let session_id = req["session_id"]
        .as_str()
        .ok_or_else(|| "session_id required".to_string())?;
    let mut checks: Vec<Value> = Vec::new();

    match rootfs_for_launch(ctx, req) {
        Ok(rootfs) => {
            checks.push(readiness_check(
            "rootfs",
            "pass",
            false,
            json!({"profile": req["profile"].clone(), "rootfs": rootfs.display().to_string(), "clone_mode": "per-session-overlay"}),
            ));
            match runtime_overlay_preflight(ctx, session_id, &rootfs) {
                Ok(preflight) => checks.push(readiness_check(
                    "runtime_rootfs_overlay",
                    "pass",
                    false,
                    preflight,
                )),
                Err(err) => checks.push(readiness_check(
                    "runtime_rootfs_overlay",
                    "fail",
                    true,
                    json!({"error": err}),
                )),
            }
        }
        Err(err) => checks.push(readiness_check(
            "rootfs",
            "fail",
            true,
            json!({"error": err}),
        )),
    }

    let guest_port = req["guest_port"].as_u64().map(|v| v as u16);
    let protocol = req["protocol"].as_str().unwrap_or("tcp");
    let allocations = load_port_allocations(ctx);
    let port_status = if req["visibility"].as_str().unwrap_or("public") == "private" {
        Ok(Value::Null)
    } else if let Some(requested) = requested_host_port(req) {
        match validate_host_port(protocol, requested, &allocations, Some(session_id)) {
            Some(reason) => Err(reason),
            None => Ok(json!(requested)),
        }
    } else {
        choose_available_port(ctx, protocol, &allocations).map(|port| json!(port))
    };
    let workspace_port_check = if req["visibility"].as_str().unwrap_or("public") == "private" {
        "workspace_port"
    } else {
        "published_workspace_port"
    };
    match (guest_port, port_status) {
        (Some(guest), Ok(host)) => checks.push(readiness_check(
            workspace_port_check,
            "pass",
            false,
            json!({"protocol": normalize_protocol(protocol), "guest_port": guest, "host_port": host}),
        )),
        (None, _) => checks.push(readiness_check(
            workspace_port_check,
            "fail",
            true,
            json!({"error": "guest_port required"}),
        )),
        (_, Err(err)) => checks.push(readiness_check(
            workspace_port_check,
            "fail",
            true,
            json!({"error": err}),
        )),
    }

    if let Some(ssh_host_port) = req.get("ssh_host_port") {
        let ssh_status = requested_host_port(&json!({"host_port": ssh_host_port}))
            .ok_or_else(|| "ssh_host_port must be explicit".to_string())
            .and_then(|port| {
                validate_host_port("tcp", port, &allocations, Some(session_id))
                    .map(Err)
                    .unwrap_or(Ok(port))
            });
        match ssh_status {
            Ok(port) => checks.push(readiness_check(
                "ssh_port",
                "pass",
                false,
                json!({"protocol": "tcp", "guest_port": 22, "host_port": port}),
            )),
            Err(err) => checks.push(readiness_check(
                "ssh_port",
                "fail",
                true,
                json!({"error": err}),
            )),
        }
    }

    let volume_mount = volume_mountpoint(ctx, session_id);
    checks.push(readiness_check(
        "workspace_storage",
        "pass",
        false,
        json!({
            "mode": "per-session-ext4",
            "image": volume_image_path(ctx, session_id).display().to_string(),
            "mountpoint": volume_mount.display().to_string(),
            "already_mounted": mounted_at(&volume_mount),
            "quota_bytes": req["storage_quota_bytes"].as_u64().unwrap_or(ctx.default_storage_quota_bytes)
        }),
    ));

    let runtime_state = read_runtime_rootfs_state(ctx, session_id);
    if runtime_state["mounted"].as_bool().unwrap_or(false) {
        checks.push(readiness_check(
            "runtime_rootfs_previous_mount",
            "warn",
            false,
            json!({"runtime_rootfs": runtime_state}),
        ));
    }

    if req["probe_r2"].as_bool().unwrap_or(false) || req["require_r2"].as_bool().unwrap_or(false) {
        match r2_probe(ctx, session_id) {
            Ok(probe) if probe["ok"].as_bool().unwrap_or(false) => checks.push(readiness_check(
                "r2",
                "pass",
                false,
                json!({"probe": probe}),
            )),
            Ok(probe) => checks.push(readiness_check(
                "r2",
                "fail",
                req["require_r2"].as_bool().unwrap_or(false),
                json!({"probe": probe}),
            )),
            Err(err) => checks.push(readiness_check(
                "r2",
                "fail",
                req["require_r2"].as_bool().unwrap_or(false),
                json!({"error": err}),
            )),
        }
    }

    if req["probe_user_s3"].as_bool().unwrap_or(false) {
        checks.push(readiness_check(
            "user_s3",
            "fail",
            true,
            json!({"error": "user-s3 storage backend is disabled; managed R2 is required"}),
        ));
    }

    match require_host_probe(ctx, req) {
        Ok(Value::Null) => checks.push(readiness_check(
            "host_probe",
            "pass",
            false,
            json!({"mode": "not_required_for_this_launch"}),
        )),
        Ok(probe) => checks.push(readiness_check(
            "host_probe",
            "pass",
            false,
            json!({"probe": probe}),
        )),
        Err(err) => checks.push(readiness_check(
            "host_probe",
            "fail",
            true,
            json!({"error": err}),
        )),
    }

    let blocking: Vec<Value> = checks
        .iter()
        .filter(|check| {
            check["blocking"].as_bool().unwrap_or(false) && check["status"].as_str() == Some("fail")
        })
        .cloned()
        .collect();
    let result = json!({
        "ok": blocking.is_empty(),
        "schema": "gema.launch_readiness.v1",
        "session_id": session_id,
        "generated_at": now(),
        "source": "hostd",
        "clone_contract": {
            "golden_rootfs_shared": true,
            "user_writable_layer": "per-session-overlay",
            "user_storage": "mounted-at-launch",
            "shared_live_desktop": false
        },
        "checks": checks,
        "blocking": blocking
    });
    if write {
        write_json_atomic(&launch_readiness_path(ctx, session_id), &result)?;
    }
    Ok(result)
}

fn path_command_exists(rootfs: &Path, command: &str) -> bool {
    [
        "opt/gema/bin",
        "usr/local/sbin",
        "usr/local/bin",
        "usr/sbin",
        "usr/bin",
        "sbin",
        "bin",
    ]
    .iter()
    .any(|prefix| rootfs.join(prefix).join(command).exists())
}

fn rootfs_checksum(rootfs: &Path) -> String {
    let mut digest = Sha256::new();
    for relative in [
        "etc/os-release",
        "etc/gema-release",
        ".gema-rootfs-ready",
        "var/lib/dpkg/status",
    ] {
        let path = rootfs.join(relative);
        if path.exists() {
            digest.update(relative.as_bytes());
            if let Ok(mut file) = File::open(path) {
                let mut buf = [0u8; 1024 * 1024];
                loop {
                    match file.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => digest.update(&buf[..n]),
                    }
                }
            }
        }
    }
    format!("sha256:{:x}", digest.finalize())
}

fn package_versions(rootfs: &Path, names: &[&str]) -> Value {
    let mut packages = Map::new();
    let wanted: BTreeSet<&str> = names.iter().copied().collect();
    let mut current: HashMap<String, String> = HashMap::new();
    let mut lines: Vec<String> = read_text(&rootfs.join("var/lib/dpkg/status"))
        .lines()
        .map(str::to_string)
        .collect();
    lines.push(String::new());
    for line in lines {
        if line.trim().is_empty() {
            if let Some(name) = current.get("Package") {
                if wanted.contains(name.as_str()) {
                    packages.insert(
                        name.clone(),
                        json!(current.get("Version").cloned().unwrap_or_default()),
                    );
                }
            }
            current.clear();
        } else if !line.starts_with(' ') {
            if let Some((k, v)) = line.split_once(':') {
                current.insert(k.to_string(), v.trim().to_string());
            }
        }
    }
    Value::Object(packages)
}

fn count_packages(rootfs: &Path) -> u64 {
    read_text(&rootfs.join("var/lib/dpkg/status"))
        .lines()
        .filter(|line| line.starts_with("Package: "))
        .count() as u64
}

fn detect_rootfs_capabilities(rootfs: &Path) -> Value {
    let gvphase1_backend = rootfs.join("opt/gema/gvphase1/bin/gvphase1-backend");
    let gvphase1_lib = rootfs.join("opt/gema/gvphase1/lib/libcuda.so.1");
    let legacy_cuda_proxy_lib = rootfs.join("opt/gema/cuda-proxy/lib/libcuda.so.1");
    let commands = json!({
        "bash": path_command_exists(rootfs, "bash"),
        "python3": path_command_exists(rootfs, "python3"),
        "curl": path_command_exists(rootfs, "curl"),
        "iproute2": path_command_exists(rootfs, "ip"),
        "sudo": path_command_exists(rootfs, "sudo"),
        "ssh": path_command_exists(rootfs, "ssh"),
        "sshd": path_command_exists(rootfs, "sshd"),
        "ssh-keygen": path_command_exists(rootfs, "ssh-keygen"),
        "ffmpeg": path_command_exists(rootfs, "ffmpeg"),
        "git": path_command_exists(rootfs, "git"),
        "jq": path_command_exists(rootfs, "jq"),
        "wget": path_command_exists(rootfs, "wget"),
        "rsync": path_command_exists(rootfs, "rsync"),
        "tmux": path_command_exists(rootfs, "tmux"),
        "htop": path_command_exists(rootfs, "htop"),
        "nano": path_command_exists(rootfs, "nano"),
        "nvidia_smi": path_command_exists(rootfs, "nvidia-smi"),
        "jupyter": path_command_exists(rootfs, "jupyter"),
        "node": path_command_exists(rootfs, "node"),
        "npm": path_command_exists(rootfs, "npm"),
        "gema-cuda-select": rootfs.join("opt/gema/bin/gema-cuda-select").exists(),
        "gema-fabric-bridge": rootfs.join("opt/gema/bin/gema-fabric-bridge").exists(),
        "gema-ml-bridge": rootfs.join("opt/gema/bin/gema-ml-bridge").exists(),
        "gema-accelerator-worker": rootfs.join("opt/gema/bin/gema-accelerator-worker").exists(),
        "gema-cluster-bridge": rootfs.join("opt/gema/bin/gema-cluster-bridge").exists(),
        "gvphase1-backend": rootfs.join("opt/gema/bin/gvphase1-backend").exists() || gvphase1_backend.exists(),
    });
    let ml_lanes = detect_ml_lanes(rootfs);
    let cuda_toolkits = detect_cuda_toolkits(rootfs);
    json!({
        "runtime": ["gema-native"],
        "os": ["linux"],
        "commands": commands,
        "desktop": {"xfce": path_command_exists(rootfs, "xfce4-session"), "pipewire": path_command_exists(rootfs, "pipewire"), "sunshine": path_command_exists(rootfs, "sunshine")},
        "gpu": {"nvidia_userland": path_command_exists(rootfs, "nvidia-smi") || cuda_toolkits.as_array().is_some_and(|items| !items.is_empty()), "cuda_path": if rootfs.join("usr/local/cuda").exists() { "/usr/local/cuda" } else { "" }, "cuda_toolkits": cuda_toolkits, "driver_required_from_host": ml_lanes.as_object().is_some_and(|lanes| !lanes.is_empty())},
        "cuda_proxy": {
            "primary": gvphase1_lib.exists() && gvphase1_backend.exists(),
            "kind": if gvphase1_lib.exists() && gvphase1_backend.exists() { "gvphase1_binary_cuda_api_remoting" } else if legacy_cuda_proxy_lib.exists() { "legacy_driver_rpc_prototype" } else { "none" },
            "frontend_library": if gvphase1_lib.exists() { "/opt/gema/gvphase1/lib/libcuda.so.1" } else if legacy_cuda_proxy_lib.exists() { "/opt/gema/cuda-proxy/lib/libcuda.so.1" } else { "" },
            "backend": if gvphase1_backend.exists() { "/opt/gema/gvphase1/bin/gvphase1-backend" } else { "" },
            "libraries": {
                "libcuda": gvphase1_lib.exists(),
                "libcudart": rootfs.join("opt/gema/gvphase1/lib/libcudart.so").exists(),
                "libcublas": rootfs.join("opt/gema/gvphase1/lib/libcublas.so").exists(),
                "libcudnn": rootfs.join("opt/gema/gvphase1/lib/libcudnn.so").exists(),
                "libcurand": rootfs.join("opt/gema/gvphase1/lib/libcurand.so").exists(),
                "libcufft": rootfs.join("opt/gema/gvphase1/lib/libcufft.so").exists(),
                "libnvrtc": rootfs.join("opt/gema/gvphase1/lib/libnvrtc.so").exists(),
                "libnvidia_ml": rootfs.join("opt/gema/gvphase1/lib/libnvidia-ml.so").exists()
            },
            "truth_role": "primary data plane for terminal-visible remote GPU when validated on an attached backend"
        },
        "ml": {"python3": path_command_exists(rootfs, "python3"), "selector": rootfs.join("opt/gema/bin/gema-cuda-select").exists(), "fabric_bridge": rootfs.join("opt/gema/bin/gema-fabric-bridge").exists(), "bridge": rootfs.join("opt/gema/bin/gema-ml-bridge").exists(), "accelerator_worker": rootfs.join("opt/gema/bin/gema-accelerator-worker").exists(), "cluster_bridge": rootfs.join("opt/gema/bin/gema-cluster-bridge").exists(), "managed_env_root": "/opt/gema/ml", "wheel_only": ml_lanes.as_object().is_some_and(|lanes| !lanes.is_empty()) && cuda_toolkits.as_array().is_some_and(|items| items.is_empty()), "lanes": ml_lanes, "jupyter": path_command_exists(rootfs, "jupyter")}
    })
}

fn detect_ml_lanes(rootfs: &Path) -> Value {
    let mut lanes = Map::new();
    let base = rootfs.join("opt/gema/ml");
    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            let lane = entry.file_name().to_string_lossy().to_string();
            if entry.path().join("bin/python").exists() {
                lanes.insert(
                    lane.clone(),
                    json!({
                        "python": format!("/opt/gema/ml/{lane}/bin/python"),
                        "freeze": entry.path().join("gema.freeze.txt").exists()
                    }),
                );
            }
        }
    }
    Value::Object(lanes)
}

fn detect_cuda_toolkits(rootfs: &Path) -> Value {
    let mut toolkits = Vec::new();
    if let Ok(entries) = fs::read_dir(rootfs.join("usr/local")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("cuda-") && entry.path().is_dir() {
                toolkits.push(json!(name.trim_start_matches("cuda-").to_string()));
            }
        }
    }
    Value::Array(toolkits)
}

fn host_gpu_report() -> Value {
    if !command_exists("nvidia-smi") {
        return json!({"vendor": null, "driver_version": null, "cuda_runtime_supported": null, "gpus": []});
    }
    let out = command_output(&[
        "nvidia-smi",
        "--query-gpu=driver_version,name,compute_cap",
        "--format=csv,noheader",
    ]);
    let mut gpus = Vec::new();
    let mut driver_version = Value::Null;
    for line in out["stdout"].as_str().unwrap_or("").lines() {
        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
        if parts.len() >= 3 {
            if driver_version.is_null() {
                driver_version = json!(parts[0]);
            }
            gpus.push(json!({"driver_version": parts[0], "name": parts[1], "compute_capability": parts[2]}));
        }
    }
    json!({"vendor": if gpus.is_empty() { Value::Null } else { json!("nvidia") }, "driver_version": driver_version, "cuda_runtime_supported": host_nvidia_cuda_version(), "gpus": gpus})
}

fn host_nvidia_cuda_version() -> Option<String> {
    if !command_exists("nvidia-smi") {
        return None;
    }
    let out = command_output(&[
        "nvidia-smi",
        "--query-gpu=cuda_version",
        "--format=csv,noheader",
    ]);
    if !out["ok"].as_bool().unwrap_or(false) {
        return None;
    }
    out["stdout"]
        .as_str()
        .and_then(|value| value.lines().next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn synthesize_image_manifest(profile_id: &str, rootfs: &Path) -> Value {
    let os_release = parse_kv_file(&rootfs.join("etc/os-release"));
    let gema_release = parse_kv_file(&rootfs.join("etc/gema-release"));
    let ready_at = read_text(&rootfs.join(".gema-rootfs-ready"));
    let capabilities = detect_rootfs_capabilities(rootfs);
    let mut profile_kind = "base";
    if capabilities["gpu"]["nvidia_userland"]
        .as_bool()
        .unwrap_or(false)
    {
        profile_kind = "gpu";
    }
    if capabilities["ml"]["lanes"]
        .as_object()
        .is_some_and(|lanes| !lanes.is_empty())
    {
        profile_kind = "gpu-ml";
    } else if capabilities["ml"]["jupyter"].as_bool().unwrap_or(false) {
        profile_kind = "ml";
    }
    json!({
        "id": profile_id,
        "name": gema_release.get("NAME").cloned().unwrap_or_else(|| format!("Gema {profile_id}")),
        "profile": profile_kind,
        "rootfs": rootfs.display().to_string(),
        "version": if ready_at.is_empty() { os_release.get("VERSION_ID").cloned().unwrap_or_else(|| "unknown".to_string()) } else { ready_at },
        "checksum": rootfs_checksum(rootfs),
        "os": {"id": os_release.get("ID"), "name": os_release.get("PRETTY_NAME"), "version_id": os_release.get("VERSION_ID"), "codename": os_release.get("VERSION_CODENAME"), "architecture": std::env::consts::ARCH},
        "capabilities": capabilities,
        "packages": {"count": count_packages(rootfs), "versions": package_versions(rootfs, &["bash", "python3", "curl", "sudo", "iproute2", "ca-certificates"])},
        "compatibility": {"runtime": ["gema-native"], "gpu_vendors": if capabilities["gpu"]["nvidia_userland"].as_bool().unwrap_or(false) { json!(["cpu", "nvidia"]) } else { json!(["cpu"]) }, "requires": {"min_nvidia_driver": null, "cuda": null, "kernel_features": ["cgroup-v2", "user-namespace", "network-namespace"]}},
        "generated_at": now()
    })
}

fn image_catalog(ctx: &Ctx) -> HResult<Value> {
    let catalog = json!({"ok": true, "generated_at": now(), "rootfs_dir": ctx.root.join("rootfs").display().to_string(), "host_gpu": host_gpu_report(), "profiles": rootfs_profiles(ctx)});
    write_json_atomic(&ctx.image_catalog_path, &catalog)?;
    Ok(catalog)
}

fn image_compatibility(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let catalog = image_catalog(ctx)?;
    let requested_profile = req["profile"].as_str();
    let requested_runtime = req["runtime"].as_str().unwrap_or("gema-native");
    let requested_gpu = req["gpu_vendor"].as_str().unwrap_or("cpu");
    let requested_cuda = req["cuda"].as_str();
    let mut matches = Vec::new();
    for profile in catalog["profiles"].as_array().cloned().unwrap_or_default() {
        let mut compatible = true;
        let mut reasons = Vec::new();
        if let Some(wanted) = requested_profile {
            if profile["id"].as_str() != Some(wanted) && profile["profile"].as_str() != Some(wanted)
            {
                compatible = false;
                reasons.push("profile_mismatch");
            }
        }
        if !profile["compatibility"]["runtime"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(requested_runtime)))
        {
            compatible = false;
            reasons.push("runtime_mismatch");
        }
        if !profile["compatibility"]["gpu_vendors"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(requested_gpu)))
        {
            compatible = false;
            reasons.push("gpu_vendor_mismatch");
        }
        if let Some(cuda) = requested_cuda {
            if !profile_supports_requested_cuda(&profile, cuda) {
                compatible = false;
                reasons.push("cuda_mismatch");
            }
        }
        matches.push(json!({"id": profile["id"].clone(), "compatible": compatible, "reasons": reasons, "profile": profile}));
    }
    Ok(
        json!({"ok": true, "request": {"profile": requested_profile, "runtime": requested_runtime, "gpu_vendor": requested_gpu, "cuda": requested_cuda}, "matches": matches}),
    )
}

fn profile_supports_requested_cuda(profile: &Value, requested: &str) -> bool {
    let Some((req_major, req_minor)) = parse_cuda_version(requested) else {
        return false;
    };
    let has_compatible_toolkit = profile["capabilities"]["gpu"]["cuda_toolkits"]
        .as_array()
        .is_some_and(|toolkits| {
            toolkits.iter().any(|toolkit| {
                toolkit
                    .as_str()
                    .and_then(parse_cuda_version)
                    .is_some_and(|(major, minor)| {
                        major < req_major || (major == req_major && minor <= req_minor)
                    })
            })
        });
    let has_compatible_python_lane = profile["capabilities"]["ml"]["lanes"]
        .as_object()
        .is_some_and(|lanes| {
            lanes.keys().any(|lane| {
                cuda_lane_version(lane).is_some_and(|(major, minor)| {
                    major < req_major || (major == req_major && minor <= req_minor)
                })
            })
        });
    has_compatible_toolkit || has_compatible_python_lane
}

fn parse_cuda_version(version: &str) -> Option<(u32, u32)> {
    let normalized = version.trim().trim_start_matches("cuda-");
    let mut parts = normalized.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}

fn cuda_lane_version(lane: &str) -> Option<(u32, u32)> {
    let digits = lane.strip_prefix("cu")?;
    match digits.len() {
        3 => {
            let major = digits[..2].parse().ok()?;
            let minor = digits[2..].parse().ok()?;
            Some((major, minor))
        }
        4 => {
            let major = digits[..2].parse().ok()?;
            let minor = digits[2..].parse().ok()?;
            Some((major, minor))
        }
        _ => None,
    }
}

fn image_cache_index_path(ctx: &Ctx) -> PathBuf {
    ctx.images_dir.join("cache-index.json")
}

fn image_rootfs_path(ctx: &Ctx, cache_key: &str) -> PathBuf {
    ctx.root.join("rootfs").join(cache_key)
}

fn image_safe_name(reference: &str) -> String {
    reference
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

fn image_unpack_to_rootfs(ctx: &Ctx, reference: &str, cache_key: &str) -> HResult<Value> {
    let rootfs = image_rootfs_path(ctx, cache_key);
    let manifest_path = rootfs.join("etc/gema-image.json");
    if rootfs.join(".gema-rootfs-ready").exists() {
        let mut manifest = read_json(&manifest_path, Value::Null);
        if manifest.is_object() && manifest["source_image"].is_null() {
            if let Some(dst) = manifest.as_object_mut() {
                dst.insert("source_image".to_string(), json!(reference));
            }
            let _ = write_json_atomic(&manifest_path, &manifest);
        }
        return Ok(
            json!({"ok": true, "reference": reference, "profile": cache_key, "rootfs": rootfs.display().to_string(), "cached": true, "manifest": read_json(&manifest_path, Value::Null)}),
        );
    }
    fs::create_dir_all(rootfs.parent().unwrap()).map_err(|err| err.to_string())?;
    let tmp_rootfs = ctx
        .root
        .join("rootfs")
        .join(format!(".{cache_key}.tmp.{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_rootfs);
    fs::create_dir_all(&tmp_rootfs).map_err(|err| err.to_string())?;

    let mut attempts = Vec::new();
    let mut unpacked = false;
    if command_exists("umoci")
        && ctx
            .image_cache_dir
            .join(cache_key)
            .join("index.json")
            .exists()
    {
        let source = format!("{}:latest", ctx.image_cache_dir.join(cache_key).display());
        let bundle = ctx
            .images_dir
            .join("unpack")
            .join(format!("{cache_key}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&bundle);
        fs::create_dir_all(&bundle).map_err(|err| err.to_string())?;
        let out = command_output(&[
            "umoci",
            "unpack",
            "--image",
            &source,
            &bundle.display().to_string(),
        ]);
        let ok = out["ok"].as_bool().unwrap_or(false);
        attempts.push(json!({"tool": "umoci", "ok": ok, "status": out["status"].clone(), "stderr": out["stderr"].clone()}));
        if ok {
            let src = bundle.join("rootfs");
            let out = command_output(&[
                "rsync",
                "-aHAX",
                "--numeric-ids",
                &format!("{}/", src.display()),
                &format!("{}/", tmp_rootfs.display()),
            ]);
            let copied = out["ok"].as_bool().unwrap_or(false);
            attempts.push(json!({"tool": "rsync", "source": src.display().to_string(), "ok": copied, "status": out["status"].clone(), "stderr": out["stderr"].clone()}));
            unpacked = copied;
        }
        let _ = fs::remove_dir_all(&bundle);
    }
    if !unpacked && command_exists("buildah") {
        let cname = format!("gema-unpack-{cache_key}-{}", std::process::id());
        let from = command_output(&["buildah", "from", "--name", &cname, reference]);
        let from_ok = from["ok"].as_bool().unwrap_or(false);
        attempts.push(json!({"tool": "buildah-from", "ok": from_ok, "status": from["status"].clone(), "stderr": from["stderr"].clone()}));
        if from_ok {
            let mount = command_output(&["buildah", "mount", &cname]);
            let mount_ok = mount["ok"].as_bool().unwrap_or(false);
            attempts.push(json!({"tool": "buildah-mount", "ok": mount_ok, "status": mount["status"].clone(), "stderr": mount["stderr"].clone()}));
            if mount_ok {
                let src = mount["stdout"].as_str().unwrap_or("").trim().to_string();
                if !src.is_empty() {
                    let out = command_output(&[
                        "rsync",
                        "-aHAX",
                        "--numeric-ids",
                        &format!("{src}/"),
                        &format!("{}/", tmp_rootfs.display()),
                    ]);
                    let copied = out["ok"].as_bool().unwrap_or(false);
                    attempts.push(json!({"tool": "rsync", "source": src, "ok": copied, "status": out["status"].clone(), "stderr": out["stderr"].clone()}));
                    unpacked = copied;
                }
                let _ = command_output(&["buildah", "umount", &cname]);
            }
            let _ = command_output(&["buildah", "rm", &cname]);
        }
    }
    if !unpacked {
        let _ = fs::remove_dir_all(&tmp_rootfs);
        return Ok(
            json!({"ok": false, "reference": reference, "profile": cache_key, "rootfs": rootfs.display().to_string(), "attempts": attempts, "error": "image could not be unpacked into a managed rootfs"}),
        );
    }
    fs::create_dir_all(tmp_rootfs.join("etc")).map_err(|err| err.to_string())?;
    fs::write(tmp_rootfs.join(".gema-rootfs-ready"), now()).map_err(|err| err.to_string())?;
    let mut manifest = synthesize_image_manifest(cache_key, &tmp_rootfs);
    if let Some(dst) = manifest.as_object_mut() {
        dst.insert("source_image".to_string(), json!(reference));
        dst.insert("unpacked_at".to_string(), json!(now()));
    }
    write_json_atomic(&tmp_rootfs.join("etc/gema-image.json"), &manifest)?;
    let _ = fs::remove_dir_all(&rootfs);
    fs::rename(&tmp_rootfs, &rootfs)
        .map_err(|err| format!("promote rootfs {}: {err}", rootfs.display()))?;
    let mut manifest = synthesize_image_manifest(cache_key, &rootfs);
    if let Some(dst) = manifest.as_object_mut() {
        dst.insert("source_image".to_string(), json!(reference));
        dst.insert("unpacked_at".to_string(), json!(now()));
    }
    write_json_atomic(&manifest_path, &manifest)?;
    Ok(
        json!({"ok": true, "reference": reference, "profile": cache_key, "rootfs": rootfs.display().to_string(), "cached": false, "attempts": attempts, "manifest": manifest}),
    )
}

fn image_verify(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let reference = req["reference"]
        .as_str()
        .or_else(|| req["image"].as_str())
        .unwrap_or("");
    let profile = req["profile"].as_str().unwrap_or("");
    if reference.is_empty() && profile.is_empty() {
        return Err("reference or profile required".to_string());
    }
    let cosign_key = req["cosign_public_key"]
        .as_str()
        .map(str::to_string)
        .or_else(|| env::var("GEMA_IMAGE_COSIGN_PUBLIC_KEY").ok())
        .filter(|v| !v.trim().is_empty());
    let mut signature = json!({"required": cosign_key.is_some(), "verified": false, "tool": "cosign", "status": null, "stderr": null});
    if let Some(key) = cosign_key {
        if !command_exists("cosign") {
            return Err(
                "cosign is required because GEMA_IMAGE_COSIGN_PUBLIC_KEY is configured".to_string(),
            );
        }
        let target = if reference.is_empty() {
            profile
        } else {
            reference
        };
        let out = command_output(&["cosign", "verify", "--key", &key, target]);
        signature = json!({"required": true, "verified": out["ok"].as_bool().unwrap_or(false), "tool": "cosign", "status": out["status"].clone(), "stderr": out["stderr"].clone()});
        if !signature["verified"].as_bool().unwrap_or(false) {
            return Ok(
                json!({"ok": false, "reference": reference, "profile": profile, "signature": signature}),
            );
        }
    }
    let catalog = image_catalog(ctx)?;
    let empty = Vec::new();
    let profile_match = catalog["profiles"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .find(|item| {
            !profile.is_empty()
                && (item["id"].as_str() == Some(profile)
                    || item["profile"].as_str() == Some(profile))
        })
        .cloned();
    Ok(
        json!({"ok": true, "reference": reference, "profile": profile, "profile_manifest": profile_match.unwrap_or(Value::Null), "signature": signature, "verified_at": now()}),
    )
}

fn image_pull(ctx: &Ctx, req: &Value) -> HResult<Value> {
    ctx.setup()?;
    let reference = req["reference"]
        .as_str()
        .or_else(|| req["image"].as_str())
        .ok_or_else(|| "reference required".to_string())?;
    let tag = image_safe_name(reference);
    let cache_path = ctx.image_cache_dir.join(&tag);
    fs::create_dir_all(&cache_path).map_err(|err| err.to_string())?;
    let verify_before_use = image_verify(ctx, req)?;
    if !verify_before_use["ok"].as_bool().unwrap_or(false) {
        return Ok(json!({"ok": false, "reference": reference, "verification": verify_before_use}));
    }
    let mut attempts = Vec::new();
    let success = if command_exists("skopeo") {
        let dest = format!("oci:{}", cache_path.display());
        let out = command_output(&[
            "skopeo",
            "copy",
            "--retry-times",
            "3",
            &format!("docker://{reference}"),
            &dest,
        ]);
        let ok = out["ok"].as_bool().unwrap_or(false);
        attempts.push(json!({"tool": "skopeo", "ok": ok, "status": out["status"].clone(), "stderr": out["stderr"].clone()}));
        ok
    } else if command_exists("buildah") {
        let out = command_output(&["buildah", "pull", reference]);
        let ok = out["ok"].as_bool().unwrap_or(false);
        attempts.push(json!({"tool": "buildah", "ok": ok, "status": out["status"].clone(), "stderr": out["stderr"].clone()}));
        ok
    } else if command_exists("docker") {
        let out = command_output(&["docker", "pull", reference]);
        let ok = out["ok"].as_bool().unwrap_or(false);
        attempts.push(json!({"tool": "docker", "ok": ok, "status": out["status"].clone(), "stderr": out["stderr"].clone()}));
        ok
    } else {
        attempts.push(json!({"tool": null, "ok": false, "stderr": "no image pull tool available"}));
        false
    };
    let rootfs = if success {
        image_unpack_to_rootfs(ctx, reference, &tag)?
    } else {
        json!({"ok": false, "error": "pull failed before unpack"})
    };
    let record = json!({"reference": reference, "cache_key": tag, "cache_path": cache_path.display().to_string(), "rootfs": rootfs, "verified": verify_before_use, "attempts": attempts, "ok": success && rootfs["ok"].as_bool().unwrap_or(false), "updated_at": now()});
    let mut index = read_json(&image_cache_index_path(ctx), json!({"images": {}}));
    index["images"]
        .as_object_mut()
        .unwrap()
        .insert(reference.to_string(), record.clone());
    write_json_atomic(&image_cache_index_path(ctx), &index)?;
    Ok(record)
}

fn image_prune(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let mut removed = Vec::new();
    let keep = req["keep"].as_array().cloned().unwrap_or_default();
    let keep_refs: BTreeSet<String> = keep
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let index = read_json(&image_cache_index_path(ctx), json!({"images": {}}));
    for (reference, record) in index["images"].as_object().cloned().unwrap_or_default() {
        if keep_refs.contains(&reference) {
            continue;
        }
        if let Some(path) = record["cache_path"].as_str() {
            let p = PathBuf::from(path);
            if p.starts_with(&ctx.image_cache_dir) && p.exists() {
                fs::remove_dir_all(&p).map_err(|err| format!("remove {}: {err}", p.display()))?;
                removed.push(json!({"reference": reference, "cache_path": path}));
            }
        }
    }
    write_json_atomic(&image_cache_index_path(ctx), &json!({"images": {}}))?;
    Ok(json!({"ok": true, "removed": removed, "at": now()}))
}

fn gpu_metrics(assignment: &Value) -> Value {
    let mut base = json!({"utilization_percent": null, "vram_used_bytes": null, "vram_total_bytes": null, "temperature_c": null, "power_watts": null, "vendor": assignment["vendor"].clone(), "device_id": assignment["device_id"].clone()});
    if assignment["vendor"].as_str() != Some("nvidia") || !command_exists("nvidia-smi") {
        return base;
    }
    let query = "index,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw";
    let out = command_output(&[
        "nvidia-smi",
        &format!("--query-gpu={query}"),
        "--format=csv,noheader,nounits",
    ]);
    if !out["ok"].as_bool().unwrap_or(false) {
        base["error"] = out["stderr"].clone();
        return base;
    }
    let wanted = assignment["device_id"].as_str().unwrap_or("0");
    for line in out["stdout"].as_str().unwrap_or("").lines() {
        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
        if parts.len() >= 6 && parts[0] == wanted {
            base["utilization_percent"] = json!(parts[1].parse::<f64>().unwrap_or(0.0));
            base["vram_used_bytes"] =
                json!((parts[2].parse::<f64>().unwrap_or(0.0) * 1024.0 * 1024.0) as u64);
            base["vram_total_bytes"] =
                json!((parts[3].parse::<f64>().unwrap_or(0.0) * 1024.0 * 1024.0) as u64);
            base["temperature_c"] = json!(parts[4].parse::<f64>().unwrap_or(0.0));
            base["power_watts"] = json!(parts[5].parse::<f64>().unwrap_or(0.0));
        }
    }
    base
}

fn gpu_assignment_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("gpu_assignment.json")
}

fn linux_dev_major(dev: u64) -> u64 {
    ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)
}

fn linux_dev_minor(dev: u64) -> u64 {
    (dev & 0xff) | ((dev >> 12) & !0xff)
}

fn device_rule_from_path(path: &Path, access: &str) -> Option<Value> {
    let meta = fs::metadata(path).ok()?;
    let kind = if meta.file_type().is_char_device() {
        "char"
    } else if meta.file_type().is_block_device() {
        "block"
    } else {
        return None;
    };
    let rdev = meta.rdev();
    Some(json!({
        "kind": kind,
        "major": linux_dev_major(rdev),
        "minor": linux_dev_minor(rdev),
        "access": access,
        "path": path.display().to_string()
    }))
}

fn collect_device_rules_from_path(path: &Path, access: &str, out: &mut Vec<Value>) {
    if let Some(rule) = device_rule_from_path(path, access) {
        out.push(rule);
        return;
    }
    if path.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                collect_device_rules_from_path(&entry.path(), access, out);
            }
        }
    }
}

fn device_allowlist(gpu_assignment: &Value) -> Value {
    let mut rules = vec![
        json!({"kind": "char", "major": 1, "minor": 3, "access": "rw", "path": "/dev/null"}),
        json!({"kind": "char", "major": 1, "minor": 5, "access": "rw", "path": "/dev/zero"}),
        json!({"kind": "char", "major": 1, "minor": 7, "access": "rw", "path": "/dev/full"}),
        json!({"kind": "char", "major": 1, "minor": 8, "access": "rw", "path": "/dev/random"}),
        json!({"kind": "char", "major": 1, "minor": 9, "access": "rw", "path": "/dev/urandom"}),
        json!({"kind": "char", "major": 5, "minor": 0, "access": "rw", "path": "/dev/tty"}),
        json!({"kind": "char", "major": 5, "minor": 2, "access": "rw", "path": "/dev/ptmx"}),
        json!({"kind": "char", "major": 136, "minor": "*", "access": "rw", "path": "/dev/pts/*"}),
    ];
    if Path::new("/dev/net/tun").exists() {
        collect_device_rules_from_path(Path::new("/dev/net/tun"), "rw", &mut rules);
    }
    if let Some(items) = gpu_assignment["device_binds"].as_array() {
        for item in items {
            if let Some(host) = item["host_path"].as_str() {
                collect_device_rules_from_path(
                    Path::new(host),
                    item["permissions"].as_str().unwrap_or("rwm"),
                    &mut rules,
                );
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut deduped = Vec::new();
    for rule in rules {
        let key = format!(
            "{}:{}:{}:{}",
            rule["kind"].as_str().unwrap_or("char"),
            rule["major"],
            rule["minor"],
            rule["access"].as_str().unwrap_or("rw")
        );
        if seen.insert(key) {
            deduped.push(rule);
        }
    }
    json!(deduped)
}

fn existing_device_bind(host_path: &str, guest_path: Option<&str>) -> Option<Value> {
    if Path::new(host_path).exists() {
        Some(
            json!({"host_path": host_path, "guest_path": guest_path.unwrap_or(host_path), "permissions": "rwm"}),
        )
    } else {
        None
    }
}

fn gpu_device_binds(vendor: &str, device_id: &str) -> Vec<Value> {
    let mut binds = Vec::new();
    if vendor == "nvidia" {
        for path in [
            format!("/dev/nvidia{device_id}"),
            "/dev/nvidiactl".to_string(),
            "/dev/nvidia-uvm".to_string(),
            "/dev/nvidia-uvm-tools".to_string(),
            "/dev/nvidia-modeset".to_string(),
        ] {
            if let Some(bind) = existing_device_bind(&path, None) {
                binds.push(bind);
            }
        }
        if Path::new("/dev/dri").exists() {
            binds.push(
                json!({"host_path": "/dev/dri", "guest_path": "/dev/dri", "permissions": "rwm"}),
            );
        }
    } else if vendor == "amd" {
        for path in ["/dev/kfd", "/dev/dri"] {
            if let Some(bind) = existing_device_bind(path, None) {
                binds.push(bind);
            }
        }
    }
    binds
}

fn device_binds_env(assignment: &Value) -> String {
    assignment["device_binds"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let host = item["host_path"].as_str()?;
                    let guest = item["guest_path"].as_str().unwrap_or(host);
                    Some(format!("{host}:{guest}"))
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

fn assign_gpu(
    ctx: &Ctx,
    session_id: &str,
    vendor: &str,
    device_id: &str,
    mode: &str,
    actor: &str,
) -> HResult<Value> {
    let vendor = if vendor == "amd" { "amd" } else { "nvidia" };
    let required_probe = host_probe(
        ctx,
        &json!({"mode": "paid-gpu", "gpu_vendor": vendor, "require_r2": false}),
    )?;
    let gpu_gate_name = if vendor == "amd" {
        "amd_gpu"
    } else {
        "nvidia_gpu"
    };
    let empty = Vec::new();
    let gate_ok = required_probe["checks"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .any(|item| item["name"] == gpu_gate_name && item["status"] == "pass");
    if !gate_ok {
        return Err(format!(
            "GPU assignment failed: {gpu_gate_name} gate did not pass"
        ));
    }
    let cdi_devices = if vendor == "nvidia"
        && (Path::new("/etc/cdi/nvidia.yaml").exists()
            || Path::new("/var/run/cdi/nvidia.yaml").exists())
    {
        json!([format!("nvidia.com/gpu={device_id}")])
    } else {
        json!([])
    };
    let assignment = json!({"session_id": session_id, "vendor": vendor, "device_id": device_id, "mode": mode, "device_binds": gpu_device_binds(vendor, device_id), "cdi_devices": cdi_devices, "assigned_at": now(), "probe": required_probe});
    write_json_atomic(&gpu_assignment_path(ctx, session_id), &assignment)?;
    append_runtime_event(
        ctx,
        session_id,
        "gpu_assigned",
        assignment.clone(),
        actor,
        "info",
    )?;
    write_recovery_plan(ctx, session_id, "gpu_assigned")?;
    Ok(assignment)
}

fn pid_alive(pid: i64) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn proc_children(pid: i64) -> Vec<i64> {
    Command::new("pgrep")
        .arg("-P")
        .arg(pid.to_string())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .filter_map(|value| value.parse::<i64>().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn proc_descendants(pid: i64) -> Vec<i64> {
    let mut out = Vec::new();
    for child in proc_children(pid) {
        out.push(child);
        out.extend(proc_descendants(child));
    }
    out
}

fn signal_pid(pid: i64, signal: i32) {
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

fn proc_cmdline(pid: i64) -> Vec<String> {
    fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|raw| {
            raw.split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn proc_status_field(pid: i64, key: &str) -> Option<String> {
    read_text(Path::new(&format!("/proc/{pid}/status")))
        .lines()
        .find_map(|line| {
            line.strip_prefix(key)
                .and_then(|rest| rest.strip_prefix(':'))
                .map(|value| value.trim().to_string())
        })
}

fn proc_ns_link(pid: i64, ns: &str) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/ns/{ns}"))
        .ok()
        .map(|path| path.display().to_string())
}

fn proc_ns_inode(pid: i64, ns: &str) -> Option<String> {
    proc_ns_link(pid, ns).and_then(|value| {
        value
            .strip_prefix(&format!("{ns}:["))
            .and_then(|rest| rest.strip_suffix(']'))
            .map(str::to_string)
    })
}

fn proc_is_pidns_init(pid: i64) -> bool {
    proc_status_field(pid, "NSpid")
        .and_then(|value| value.split_whitespace().last().map(str::to_string))
        .is_some_and(|inner| inner == "1")
}

fn resolve_checkpoint_tree_pid(ctx: &Ctx, session_id: &str, requested_pid: i64) -> HResult<i64> {
    if !pid_alive(requested_pid) {
        return Err(format!("process is not alive: {requested_pid}"));
    }
    if proc_is_pidns_init(requested_pid) {
        return Ok(requested_pid);
    }
    let wanted_ns = proc_ns_link(requested_pid, "pid")
        .ok_or_else(|| format!("cannot inspect pid namespace for process: {requested_pid}"))?;
    let registry = read_process_registry(ctx, session_id);
    registry["active"]
        .as_object()
        .into_iter()
        .flat_map(|items| items.values())
        .filter(|record| record["kind"].as_str() == Some("workload"))
        .filter_map(|record| record["pid"].as_i64())
        .find(|pid| {
            pid_alive(*pid)
                && proc_is_pidns_init(*pid)
                && proc_ns_link(*pid, "pid").as_deref() == Some(wanted_ns.as_str())
        })
        .ok_or_else(|| {
            format!("no checkpointable pid namespace root found for process: {requested_pid}")
        })
}

fn process_supervisor_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("processes.json")
}

fn process_checkpoint_manifest_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("process_checkpoints.json")
}

fn process_checkpoints_dir(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("process-checkpoints")
}

fn read_process_registry(ctx: &Ctx, session_id: &str) -> Value {
    read_json(
        &process_supervisor_path(ctx, session_id),
        json!({"session_id": session_id, "active": {}, "history": []}),
    )
}

fn read_process_checkpoint_manifest(ctx: &Ctx, session_id: &str) -> Value {
    read_json(
        &process_checkpoint_manifest_path(ctx, session_id),
        json!({"session_id": session_id, "checkpoints": []}),
    )
}

fn write_process_checkpoint_manifest(ctx: &Ctx, session_id: &str, mut doc: Value) -> HResult<()> {
    doc["session_id"] = json!(session_id);
    if !doc["checkpoints"].is_array() {
        doc["checkpoints"] = json!([]);
    }
    doc["updated_at"] = json!(now());
    write_json_atomic(&process_checkpoint_manifest_path(ctx, session_id), &doc)
}

fn write_process_registry(ctx: &Ctx, session_id: &str, mut doc: Value) -> HResult<()> {
    doc["session_id"] = json!(session_id);
    if doc.get("active").is_none() {
        doc["active"] = json!({});
    }
    if doc.get("history").is_none() {
        doc["history"] = json!([]);
    }
    doc["updated_at"] = json!(now());
    write_json_atomic(&process_supervisor_path(ctx, session_id), &doc)
}

fn process_key(kind: &str, pid: i64) -> String {
    format!("{kind}:{pid}")
}

fn register_process(
    ctx: &Ctx,
    session_id: &str,
    kind: &str,
    pid: i64,
    command: Value,
    restart_policy: &str,
    log_path: Value,
    role: &str,
    metadata: Value,
) -> HResult<Value> {
    let key = process_key(kind, pid);
    let record = json!({"id": key, "session_id": session_id, "kind": kind, "role": role, "pid": pid, "command": command, "status": if pid_alive(pid) { "running" } else { "unknown" }, "exit_code": null, "crash_reason": null, "restart_policy": restart_policy, "started_at": now(), "finished_at": null, "log_path": log_path, "metadata": metadata});
    let mut doc = read_process_registry(ctx, session_id);
    doc["active"]
        .as_object_mut()
        .unwrap()
        .insert(key, record.clone());
    write_process_registry(ctx, session_id, doc)?;
    append_runtime_event(
        ctx,
        session_id,
        "process_started",
        record.clone(),
        "system",
        "info",
    )?;
    Ok(record)
}

fn classify_crash(exit_code: Option<i32>, reason: Option<&str>) -> String {
    if let Some(reason) = reason {
        return reason.to_string();
    }
    match exit_code {
        None => "unknown".to_string(),
        Some(0) => "clean_exit".to_string(),
        Some(137) => "sigkill_or_oom".to_string(),
        Some(143) => "sigterm".to_string(),
        Some(code) if code < 0 => format!("signal_{}", -code),
        Some(_) => "non_zero_exit".to_string(),
    }
}

fn finish_process(
    ctx: &Ctx,
    session_id: &str,
    kind: &str,
    pid: i64,
    exit_code: Option<i32>,
    reason: Option<&str>,
    expected: bool,
) -> HResult<Value> {
    let key = process_key(kind, pid);
    let mut doc = read_process_registry(ctx, session_id);
    let mut record = doc["active"].as_object_mut().and_then(|m| m.remove(&key)).unwrap_or_else(|| json!({"id": key, "session_id": session_id, "kind": kind, "role": kind, "pid": pid, "command": proc_cmdline(pid), "restart_policy": "never", "started_at": null, "log_path": null, "metadata": {}}));
    record["status"] = json!(if expected || exit_code == Some(0) {
        "exited"
    } else {
        "crashed"
    });
    record["exit_code"] = exit_code.map_or(Value::Null, |c| json!(c));
    record["crash_reason"] = json!(classify_crash(exit_code, reason));
    record["finished_at"] = json!(now());
    doc["history"].as_array_mut().unwrap().push(record.clone());
    write_process_registry(ctx, session_id, doc)?;
    append_runtime_event(
        ctx,
        session_id,
        "process_supervisor_exit",
        record.clone(),
        "system",
        if expected || exit_code == Some(0) {
            "info"
        } else {
            "error"
        },
    )?;
    Ok(record)
}

fn cgroup_pids(session_id: &str) -> Vec<i64> {
    read_text(Path::new(&format!(
        "/sys/fs/cgroup/gema-system/{session_id}/cgroup.procs"
    )))
    .lines()
    .filter_map(|line| line.trim().parse().ok())
    .collect()
}

fn session_cgroup_has_live_workload(session_id: &str) -> bool {
    cgroup_pids(session_id).into_iter().any(pid_alive)
}

fn session_registry_has_live_workload(ctx: &Ctx, session_id: &str) -> bool {
    read_process_registry(ctx, session_id)["active"]
        .as_object()
        .map(|active| {
            active.values().any(|record| {
                let is_workload = matches!(
                    record["role"].as_str(),
                    Some("session_process") | Some("workload")
                ) || record["kind"].as_str() == Some("workload");
                is_workload && record["pid"].as_i64().is_some_and(pid_alive)
            })
        })
        .unwrap_or(false)
}

fn session_has_live_workload(ctx: &Ctx, session_id: &str) -> bool {
    session_cgroup_has_live_workload(session_id)
        || session_registry_has_live_workload(ctx, session_id)
}

fn read_ports_jsonl(ctx: &Ctx, id: &str) -> Vec<Value> {
    let path = ctx.session_dir(id).join("ports.jsonl");
    read_text(&path)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn scan_session_processes(ctx: &Ctx, session_id: &str) -> HResult<Value> {
    let mut doc = read_process_registry(ctx, session_id);
    let active_snapshot = doc["active"].as_object().cloned().unwrap_or_default();
    for (_key, record) in active_snapshot {
        if let Some(pid) = record["pid"].as_i64() {
            if !pid_alive(pid) {
                let _ = finish_process(
                    ctx,
                    session_id,
                    record["kind"].as_str().unwrap_or("process"),
                    pid,
                    None,
                    Some("process_missing"),
                    false,
                );
            }
        }
    }
    let hostd_doc = read_json(&ctx.session_dir(session_id).join("hostd.json"), json!({}));
    if let Some(pid) = hostd_doc["pid"].as_i64() {
        if pid_alive(pid)
            && !read_process_registry(ctx, session_id)["active"]
                .get(process_key("main", pid))
                .is_some()
        {
            let cmd = hostd_doc["command"].clone();
            let _ = register_process(
                ctx,
                session_id,
                "main",
                pid,
                json!([ctx.substrate.display().to_string(), "run", "--", cmd]),
                hostd_doc["restart_policy"].as_str().unwrap_or("never"),
                json!(ctx
                    .session_dir(session_id)
                    .join("logs/hostd-launch.log")
                    .display()
                    .to_string()),
                "substrate",
                json!({"guest_port": hostd_doc["guest_port"].clone(), "host_port": hostd_doc["host_port"].clone()}),
            );
        }
    }
    for port in read_ports_jsonl(ctx, session_id) {
        if let Some(pid) = port["pid"].as_i64() {
            let key = process_key("service", pid);
            if pid_alive(pid)
                && !read_process_registry(ctx, session_id)["active"]
                    .get(key)
                    .is_some()
            {
                let host_port = port["host_port"].clone();
                let guest_port = port["guest_port"].clone();
                let _ = register_process(
                    ctx,
                    session_id,
                    "service",
                    pid,
                    json!([
                        "gema-runtime",
                        "hostd",
                        "router",
                        port["protocol"].clone(),
                        host_port,
                        port["guest_ip"].clone(),
                        guest_port
                    ]),
                    "session",
                    json!(ctx
                        .session_dir(session_id)
                        .join(format!(
                            "logs/router-{}-{}.log",
                            port["host_port"], port["guest_port"]
                        ))
                        .display()
                        .to_string()),
                    "port_router",
                    port,
                );
            }
        }
    }
    for pid in cgroup_pids(session_id) {
        let key = process_key("workload", pid);
        if pid_alive(pid)
            && !read_process_registry(ctx, session_id)["active"]
                .get(key)
                .is_some()
        {
            let _ = register_process(
                ctx,
                session_id,
                "workload",
                pid,
                json!(proc_cmdline(pid)),
                "never",
                json!(ctx
                    .session_dir(session_id)
                    .join("logs/hostd-launch.log")
                    .display()
                    .to_string()),
                "session_process",
                json!({}),
            );
        }
    }
    doc = read_process_registry(ctx, session_id);
    Ok(doc)
}

fn process_checkpoint_size_bytes(path: &Path) -> u64 {
    directory_size_bytes(path)
}

fn select_checkpoint_pid(ctx: &Ctx, session_id: &str, req: &Value) -> HResult<i64> {
    if let Some(pid) = req["pid"].as_i64() {
        if !pid_alive(pid) {
            return Err(format!("process is not alive: {pid}"));
        }
        return Ok(pid);
    }
    let processes = scan_session_processes(ctx, session_id)?;
    let active = processes["active"]
        .as_object()
        .ok_or_else(|| "no active process registry".to_string())?;
    active
        .values()
        .filter(|record| {
            record["kind"].as_str() == Some("workload")
                && record["pid"].as_i64().is_some_and(pid_alive)
        })
        .filter_map(|record| record["pid"].as_i64())
        .next()
        .ok_or_else(|| "no live checkpointable workload process found".to_string())
}

fn process_checkpoint_create(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let session_id = req["session_id"].as_str().ok_or("session_id required")?;
    if !command_exists("criu") {
        return Err("criu is not installed".to_string());
    }
    let requested_pid = select_checkpoint_pid(ctx, session_id, req)?;
    let pid = resolve_checkpoint_tree_pid(ctx, session_id, requested_pid)?;
    let checkpoint_id = req["checkpoint_id"]
        .as_str()
        .filter(|v| !v.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "proc-{}-{}",
                Local::now().format("%Y%m%d%H%M%S"),
                &Uuid::new_v4().to_string()[..8]
            )
        });
    let dir = process_checkpoints_dir(ctx, session_id).join(&checkpoint_id);
    if dir.exists() {
        return Err(format!(
            "process checkpoint already exists: {checkpoint_id}"
        ));
    }
    fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    append_runtime_event(
        ctx,
        session_id,
        "process_checkpoint_started",
        json!({"checkpoint_id": checkpoint_id, "pid": pid, "dir": dir.display().to_string()}),
        req["actor"].as_str().unwrap_or("system"),
        "info",
    )?;
    let dump_log = dir.join("dump.log");
    let net_inode = proc_ns_inode(pid, "net");
    let mut dump = Command::new("criu");
    dump.arg("dump")
        .arg("-t")
        .arg(pid.to_string())
        .arg("-D")
        .arg(&dir)
        .arg("--leave-running")
        .arg("--shell-job")
        .arg("--file-locks")
        .arg("--tcp-close")
        .arg("--link-remap")
        .arg("--manage-cgroups=ignore")
        .arg("--external")
        .arg("mnt[]:m");
    if let Some(inode) = &net_inode {
        dump.arg("--external")
            .arg(format!("net[{inode}]:gema-session-net"));
    }
    let out = dump
        .arg("-o")
        .arg(&dump_log)
        .output()
        .map_err(|err| err.to_string())?;
    let ok = out.status.success();
    let size = process_checkpoint_size_bytes(&dir);
    let record = json!({
        "id": checkpoint_id,
        "status": if ok { "completed" } else { "failed" },
        "pid": pid,
        "requested_pid": requested_pid,
        "process": {"cmdline": proc_cmdline(pid), "alive": pid_alive(pid)},
        "requested_process": {"cmdline": proc_cmdline(requested_pid), "alive": pid_alive(requested_pid)},
        "external_net": net_inode.as_ref().map(|inode| json!({"inode": inode, "name": "gema-session-net"})),
        "dir": dir.display().to_string(),
        "dump_log": dump_log.display().to_string(),
        "bytes": size,
        "created_at": now(),
        "criu": {
            "status": out.status.code(),
            "stdout": String::from_utf8_lossy(&out.stdout).to_string(),
            "stderr": String::from_utf8_lossy(&out.stderr).to_string()
        }
    });
    let mut manifest = read_process_checkpoint_manifest(ctx, session_id);
    manifest["checkpoints"]
        .as_array_mut()
        .unwrap()
        .push(record.clone());
    write_process_checkpoint_manifest(ctx, session_id, manifest)?;
    append_runtime_event(
        ctx,
        session_id,
        if ok {
            "process_checkpoint_completed"
        } else {
            "process_checkpoint_failed"
        },
        record.clone(),
        req["actor"].as_str().unwrap_or("system"),
        if ok { "info" } else { "error" },
    )?;
    if ok {
        Ok(json!({"ok": true, "checkpoint": record}))
    } else {
        Err(format!(
            "process checkpoint failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn latest_completed_process_checkpoint(ctx: &Ctx, session_id: &str) -> Option<Value> {
    read_process_checkpoint_manifest(ctx, session_id)["checkpoints"]
        .as_array()?
        .iter()
        .rev()
        .find(|item| item["status"] == "completed")
        .cloned()
}

fn terminate_checkpoint_tree(ctx: &Ctx, session_id: &str, checkpoint: &Value) -> HResult<Value> {
    let pid = checkpoint["pid"]
        .as_i64()
        .ok_or_else(|| "checkpoint pid missing".to_string())?;
    if !pid_alive(pid) {
        return Ok(json!({"pid": pid, "already_stopped": true, "terminated": []}));
    }
    let mut targets = proc_descendants(pid);
    targets.push(pid);
    targets.sort_unstable();
    targets.dedup();
    for target in targets.iter().rev() {
        signal_pid(*target, libc::SIGTERM);
    }
    for _ in 0..20 {
        if targets.iter().all(|target| !pid_alive(*target)) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let mut killed = Vec::new();
    for target in targets
        .iter()
        .rev()
        .copied()
        .filter(|target| pid_alive(*target))
    {
        signal_pid(target, libc::SIGKILL);
        killed.push(target);
    }
    for _ in 0..20 {
        if targets.iter().all(|target| !pid_alive(*target)) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let still_alive: Vec<i64> = targets
        .iter()
        .copied()
        .filter(|target| pid_alive(*target))
        .collect();
    let _ = scan_session_processes(ctx, session_id);
    if !still_alive.is_empty() {
        return Err(format!(
            "checkpoint source process tree still alive after termination: {:?}",
            still_alive
        ));
    }
    Ok(json!({"pid": pid, "terminated": targets, "sigkill": killed}))
}

fn process_checkpoint_restore(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let session_id = req["session_id"].as_str().ok_or("session_id required")?;
    if !command_exists("criu") {
        return Err("criu is not installed".to_string());
    }
    let checkpoint = if let Some(id) = req["checkpoint_id"].as_str() {
        read_process_checkpoint_manifest(ctx, session_id)["checkpoints"]
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .rev()
                    .find(|item| item["id"].as_str() == Some(id))
            })
            .cloned()
            .ok_or_else(|| format!("process checkpoint not found: {id}"))?
    } else {
        latest_completed_process_checkpoint(ctx, session_id)
            .ok_or_else(|| "no completed process checkpoint found".to_string())?
    };
    if checkpoint["status"].as_str() != Some("completed") {
        return Err("process checkpoint is not completed".to_string());
    }
    let dir = checkpoint["dir"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| "process checkpoint dir missing".to_string())?;
    if !dir.exists() {
        return Err(format!("process checkpoint dir missing: {}", dir.display()));
    }
    let source_pid = checkpoint["pid"].as_i64();
    let source_alive = source_pid.is_some_and(pid_alive);
    let terminate_existing = req["terminate_existing"].as_bool().unwrap_or(false);
    let allow_live_restore = req["allow_live_restore"].as_bool().unwrap_or(false);
    let terminated_source = if source_alive && terminate_existing {
        Some(terminate_checkpoint_tree(ctx, session_id, &checkpoint)?)
    } else {
        None
    };
    if source_alive && !terminate_existing && !allow_live_restore {
        return Err(
            "checkpoint source is still running; pass terminate-existing for recovery restore or allow-live for clone restore"
                .to_string(),
        );
    }
    let meta = read_json(&ctx.session_dir(session_id).join("meta.json"), json!({}));
    let rootfs = meta["rootfs"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| "session rootfs missing from metadata".to_string())?;
    if !rootfs.exists() {
        return Err(format!("session rootfs missing: {}", rootfs.display()));
    }
    let restore_log = dir.join("restore.log");
    let pidfile = dir.join("restore.pid");
    append_runtime_event(
        ctx,
        session_id,
        "process_restore_started",
        json!({"checkpoint": checkpoint, "dir": dir.display().to_string()}),
        req["actor"].as_str().unwrap_or("system"),
        "info",
    )?;
    let netns_path = PathBuf::from(format!("/var/run/netns/gema-{session_id}"));
    let has_external_net = checkpoint["external_net"].is_object();
    let out = if has_external_net {
        if !netns_path.exists() {
            return Err(format!(
                "session network namespace missing: {}",
                netns_path.display()
            ));
        }
        Command::new("bash")
            .arg("-lc")
            .arg("exec 9<\"$GEMA_NETNS\"; exec criu restore -D \"$GEMA_CRIU_DIR\" -r \"$GEMA_ROOTFS\" --restore-detached --shell-job --file-locks --tcp-close --link-remap --manage-cgroups=ignore --external 'mnt[]' --inherit-fd 'fd[9]:gema-session-net' --pidfile \"$GEMA_PIDFILE\" -o \"$GEMA_RESTORE_LOG\"")
            .env("GEMA_NETNS", &netns_path)
            .env("GEMA_CRIU_DIR", &dir)
            .env("GEMA_ROOTFS", &rootfs)
            .env("GEMA_PIDFILE", &pidfile)
            .env("GEMA_RESTORE_LOG", &restore_log)
            .output()
            .map_err(|err| err.to_string())?
    } else {
        Command::new("criu")
            .arg("restore")
            .arg("-D")
            .arg(&dir)
            .arg("-r")
            .arg(&rootfs)
            .arg("--restore-detached")
            .arg("--shell-job")
            .arg("--file-locks")
            .arg("--tcp-close")
            .arg("--link-remap")
            .arg("--manage-cgroups=ignore")
            .arg("--external")
            .arg("mnt[]")
            .arg("--pidfile")
            .arg(&pidfile)
            .arg("-o")
            .arg(&restore_log)
            .output()
            .map_err(|err| err.to_string())?
    };
    let ok = out.status.success();
    let restored_pid = read_text(&pidfile).trim().parse::<i64>().ok();
    if let Some(pid) = restored_pid.filter(|pid| pid_alive(*pid)) {
        let _ = register_process(
            ctx,
            session_id,
            "workload",
            pid,
            json!(proc_cmdline(pid)),
            "never",
            json!(restore_log.display().to_string()),
            "restored_process",
            json!({"checkpoint_id": checkpoint["id"].clone()}),
        );
    }
    let result = json!({
        "ok": ok,
        "checkpoint": checkpoint,
        "restore_log": restore_log.display().to_string(),
        "pidfile": pidfile.display().to_string(),
        "pid": restored_pid,
        "policy": {
            "source_pid": source_pid,
            "source_was_alive": source_alive,
            "terminate_existing": terminate_existing,
            "allow_live_restore": allow_live_restore,
            "terminated_source": terminated_source
        },
        "criu": {
            "status": out.status.code(),
            "stdout": String::from_utf8_lossy(&out.stdout).to_string(),
            "stderr": String::from_utf8_lossy(&out.stderr).to_string()
        },
        "restored_at": now()
    });
    append_runtime_event(
        ctx,
        session_id,
        if ok {
            "process_restore_completed"
        } else {
            "process_restore_failed"
        },
        result.clone(),
        req["actor"].as_str().unwrap_or("system"),
        if ok { "info" } else { "error" },
    )?;
    if ok {
        Ok(result)
    } else {
        Err(format!(
            "process restore failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn storage_policy_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("storage_policy.json")
}

fn storage_latest_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("storage.latest.json")
}

fn storage_samples_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("storage.samples.jsonl")
}

fn storage_volume_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("storage_volume.json")
}

fn volume_image_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.volumes_dir.join(id).join("workspace.ext4")
}

fn volume_mountpoint(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.workspace_dir(id)
}

fn mounted_at(path: &Path) -> bool {
    Command::new("findmnt")
        .arg("-rn")
        .arg("-M")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn mounted_fstype(path: &Path) -> Option<String> {
    let out = Command::new("findmnt")
        .arg("-rn")
        .arg("-M")
        .arg(path)
        .arg("-o")
        .arg("FSTYPE")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let fstype = String::from_utf8_lossy(&out.stdout).trim().to_string();
    fstype
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn rclone_mounted_at(path: &Path) -> bool {
    mounted_fstype(path)
        .map(|fstype| {
            fstype
                .lines()
                .any(|line| matches!(line.trim(), "fuse.rclone" | "rclone"))
        })
        .unwrap_or(false)
}

fn rclone_mount_healthy(path: &Path) -> bool {
    if !rclone_mounted_at(path) {
        return false;
    }
    Command::new("timeout")
        .arg("3")
        .arg("ls")
        .arg("-la")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn detach_mount_lazy(path: &Path) {
    if !mounted_at(path) {
        return;
    }
    if command_exists("fusermount3") {
        let _ = Command::new("fusermount3").arg("-uz").arg(path).status();
    } else if command_exists("fusermount") {
        let _ = Command::new("fusermount").arg("-uz").arg(path).status();
    }
    for _ in 0..3 {
        if !mounted_at(path) {
            return;
        }
        let _ = Command::new("umount").arg("-l").arg(path).status();
        thread::sleep(Duration::from_millis(100));
    }
}

fn detach_stale_rclone_mount(path: &Path) {
    if rclone_mounted_at(path) && !rclone_mount_healthy(path) {
        detach_mount_lazy(path);
    }
}

fn prepare_r2_mountpoint(path: &Path) -> HResult<()> {
    detach_stale_rclone_mount(path);
    fs::create_dir_all(path).map_err(|err| err.to_string())?;
    if !rclone_mounted_at(path) {
        let entries = fs::read_dir(path)
            .map_err(|err| err.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?;
        if !entries.is_empty() {
            let parent = path
                .parent()
                .ok_or_else(|| format!("mountpoint has no parent: {}", path.display()))?;
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            let quarantine = parent.join(format!("workspace.local-fallback.{stamp}"));
            fs::create_dir_all(&quarantine).map_err(|err| err.to_string())?;
            for entry in entries {
                let dest = quarantine.join(entry.file_name());
                fs::rename(entry.path(), dest).map_err(|err| err.to_string())?;
            }
            lock_r2_mountpoint(&quarantine);
        }
    }
    let _ = Command::new("chown").arg("root:root").arg(path).status();
    let _ = Command::new("chmod").arg("0555").arg(path).status();
    Ok(())
}

fn lock_r2_mountpoint(path: &Path) {
    detach_stale_rclone_mount(path);
    if !rclone_mount_healthy(path) {
        let _ = Command::new("chown")
            .arg("-R")
            .arg("root:root")
            .arg(path)
            .status();
        let _ = Command::new("find")
            .arg(path)
            .arg("-type")
            .arg("d")
            .arg("-exec")
            .arg("chmod")
            .arg("0555")
            .arg("{}")
            .arg("+")
            .status();
        let _ = Command::new("find")
            .arg(path)
            .arg("-type")
            .arg("f")
            .arg("-exec")
            .arg("chmod")
            .arg("0444")
            .arg("{}")
            .arg("+")
            .status();
    }
}

fn default_storage_policy(ctx: &Ctx, session_id: &str) -> Value {
    json!({"session_id": session_id, "quota_bytes": ctx.default_storage_quota_bytes, "snapshot_retention_count": ctx.snapshot_retention_count, "cleanup_policy": {"delete_tmp": true, "delete_cache": false, "delete_old_snapshots": true}, "enforcement": {"mode": "mount-backed-ext4", "on_exceeded": "kernel_write_failure", "kernel_quota": true}, "created_at": now()})
}

fn read_storage_policy(ctx: &Ctx, session_id: &str) -> HResult<Value> {
    let existing = read_json(&storage_policy_path(ctx, session_id), Value::Null);
    if existing.is_object() {
        return Ok(existing);
    }
    let policy = default_storage_policy(ctx, session_id);
    write_json_atomic(&storage_policy_path(ctx, session_id), &policy)?;
    Ok(policy)
}

fn set_storage_policy(
    ctx: &Ctx,
    session_id: &str,
    quota_bytes: Option<u64>,
    retention: Option<u64>,
    cleanup_policy: Option<Value>,
) -> HResult<Value> {
    let mut policy = read_storage_policy(ctx, session_id)?;
    if let Some(q) = quota_bytes {
        policy["quota_bytes"] = json!(q);
    }
    if let Some(r) = retention {
        policy["snapshot_retention_count"] = json!(r);
    }
    if let Some(cleanup) = cleanup_policy {
        if cleanup.is_object() {
            let target = policy["cleanup_policy"].as_object_mut().unwrap();
            for (k, v) in cleanup.as_object().unwrap() {
                target.insert(k.clone(), v.clone());
            }
        }
    }
    policy["updated_at"] = json!(now());
    write_json_atomic(&storage_policy_path(ctx, session_id), &policy)?;
    append_runtime_event(
        ctx,
        session_id,
        "storage_policy_updated",
        policy.clone(),
        "system",
        "info",
    )?;
    Ok(policy)
}

fn read_storage_volume(ctx: &Ctx, id: &str) -> Value {
    read_json(&storage_volume_path(ctx, id), json!({}))
}

fn write_storage_volume(ctx: &Ctx, id: &str, volume: &Value) -> HResult<()> {
    write_json_atomic(&storage_volume_path(ctx, id), volume)
}

fn prepare_workspace_volume(
    ctx: &Ctx,
    session_id: &str,
    quota_bytes: Option<u64>,
) -> HResult<Value> {
    let policy = read_storage_policy(ctx, session_id)?;
    let image_path = volume_image_path(ctx, session_id);
    let mountpoint = volume_mountpoint(ctx, session_id);
    let quota = quota_bytes
        .or_else(|| policy["quota_bytes"].as_u64())
        .unwrap_or(ctx.default_storage_quota_bytes)
        .max(16 * 1024 * 1024);
    fs::create_dir_all(image_path.parent().unwrap()).map_err(|err| err.to_string())?;
    fs::create_dir_all(&mountpoint).map_err(|err| err.to_string())?;
    let mut seeded = false;
    let mut seed_dir = None;
    if !image_path.exists() {
        if fs::read_dir(&mountpoint)
            .map(|mut i| i.next().is_some())
            .unwrap_or(false)
        {
            let seed = ctx
                .volumes_dir
                .join(session_id)
                .join(format!("seed-{}", Uuid::new_v4()));
            fs::create_dir_all(seed.parent().unwrap()).map_err(|err| err.to_string())?;
            let _ = Command::new("cp")
                .arg("-a")
                .arg(format!("{}/.", mountpoint.display()))
                .arg(&seed)
                .status();
            let _ = fs::remove_dir_all(&mountpoint);
            fs::create_dir_all(&mountpoint).map_err(|err| err.to_string())?;
            seed_dir = Some(seed);
        }
        run_status(
            Command::new("truncate")
                .arg("-s")
                .arg(quota.to_string())
                .arg(&image_path),
        )?;
        run_status(
            Command::new("mkfs.ext4")
                .arg("-F")
                .arg("-q")
                .arg(&image_path),
        )?;
        let _ = Command::new("tune2fs")
            .arg("-m")
            .arg("0")
            .arg(&image_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    if image_path.metadata().map(|m| m.len()).unwrap_or(0) < quota {
        run_status(
            Command::new("truncate")
                .arg("-s")
                .arg(quota.to_string())
                .arg(&image_path),
        )?;
        let _ = Command::new("e2fsck")
            .arg("-fy")
            .arg(&image_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("resize2fs")
            .arg(&image_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    if !mounted_at(&mountpoint) {
        run_status(
            Command::new("mount")
                .arg("-o")
                .arg("loop,rw,nosuid,nodev")
                .arg(&image_path)
                .arg(&mountpoint),
        )?;
    }
    if let Some(seed) = seed_dir {
        let _ = Command::new("cp")
            .arg("-a")
            .arg(format!("{}/.", seed.display()))
            .arg(&mountpoint)
            .status();
        let _ = fs::remove_dir_all(seed);
        seeded = true;
    }
    fs::create_dir_all(mountpoint.join(".gema")).ok();
    let _ = Command::new("chown")
        .arg("-R")
        .arg(format!(
            "{}:{}",
            session_guest_uid(session_id),
            session_guest_gid(session_id)
        ))
        .arg(&mountpoint)
        .status();
    let volume = json!({"session_id": session_id, "mode": "mount-backed-ext4", "image_path": image_path.display().to_string(), "mountpoint": mountpoint.display().to_string(), "quota_bytes": quota, "image_bytes": image_path.metadata().map(|m| m.len()).unwrap_or(0), "mounted": mounted_at(&mountpoint), "seeded_from_existing_workspace": seeded, "filesystem": "ext4", "updated_at": now()});
    write_storage_volume(ctx, session_id, &volume)?;
    append_runtime_event(
        ctx,
        session_id,
        "storage_volume_mounted",
        volume.clone(),
        "system",
        "info",
    )?;
    Ok(volume)
}

fn unmount_workspace_volume(ctx: &Ctx, session_id: &str, delete: bool) -> HResult<Value> {
    let mut volume = read_storage_volume(ctx, session_id);
    let r2_workspace = r2_mountpoint(ctx, session_id, "workspace");
    detach_stale_rclone_mount(&r2_workspace);
    if rclone_mounted_at(&r2_workspace) {
        let _ = r2_unmount(ctx, session_id, "workspace");
        if rclone_mounted_at(&r2_workspace) && !rclone_mount_healthy(&r2_workspace) {
            detach_mount_lazy(&r2_workspace);
        }
    }
    let mountpoint = PathBuf::from(
        volume["mountpoint"]
            .as_str()
            .unwrap_or(&volume_mountpoint(ctx, session_id).display().to_string()),
    );
    let image_path = PathBuf::from(
        volume["image_path"]
            .as_str()
            .unwrap_or(&volume_image_path(ctx, session_id).display().to_string()),
    );
    if mounted_at(&mountpoint) {
        let _ = Command::new("umount").arg(&mountpoint).status();
        if mounted_at(&mountpoint) {
            let _ = Command::new("umount").arg("-l").arg(&mountpoint).status();
        }
    }
    if rclone_mounted_at(&mountpoint) && !rclone_mount_healthy(&mountpoint) {
        detach_mount_lazy(&mountpoint);
    }
    volume["mounted"] = json!(mounted_at(&mountpoint));
    volume["unmounted_at"] = json!(now());
    if delete && image_path.exists() {
        match fs::remove_file(&image_path) {
            Ok(_) => volume["deleted"] = json!(true),
            Err(err) => volume["delete_error"] = json!(err.to_string()),
        }
    }
    write_storage_volume(ctx, session_id, &volume)?;
    append_runtime_event(
        ctx,
        session_id,
        "storage_volume_unmounted",
        volume.clone(),
        "system",
        "info",
    )?;
    Ok(volume)
}

fn run_status(cmd: &mut Command) -> HResult<()> {
    let status = cmd.status().map_err(|err| err.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("command failed with status {status}"))
    }
}

fn session_workspace_path(ctx: &Ctx, session_id: &str) -> PathBuf {
    let meta = read_json(&ctx.session_dir(session_id).join("meta.json"), json!({}));
    meta["workspace"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.workspace_dir(session_id))
}

fn workspace_write_probe(session_id: &str, workspace: &Path) -> Value {
    if !rclone_mounted_at(workspace) {
        return json!({"ok": false, "error": "workspace is not mounted through R2; refusing local write probe", "session_id": session_id, "fstype": mounted_fstype(workspace)});
    }
    let dir = workspace.join(".gema");
    let path = dir.join(format!(
        "write-probe-{}-{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ));
    match fs::create_dir_all(&dir)
        .and_then(|_| fs::write(&path, b"gema-write-probe\n"))
        .and_then(|_| fs::remove_file(&path))
    {
        Ok(_) => json!({"ok": true, "error": null}),
        Err(err) => json!({"ok": false, "error": err.to_string(), "session_id": session_id}),
    }
}

fn snapshot_manifest_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("snapshots.json")
}

fn recovery_plan_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("recovery_plan.json")
}

fn read_snapshot_manifest(ctx: &Ctx, session_id: &str) -> Value {
    read_json(
        &snapshot_manifest_path(ctx, session_id),
        json!({"session_id": session_id, "snapshots": []}),
    )
}

fn write_snapshot_manifest(ctx: &Ctx, session_id: &str, mut manifest: Value) -> HResult<()> {
    manifest["session_id"] = json!(session_id);
    if !manifest["snapshots"].is_array() {
        manifest["snapshots"] = json!([]);
    }
    manifest["updated_at"] = json!(now());
    write_json_atomic(&snapshot_manifest_path(ctx, session_id), &manifest)
}

fn snapshot_size_from_details(details: &Value) -> u64 {
    for key in ["snapshot_bytes", "bytes", "size_bytes"] {
        if let Some(value) = details[key].as_u64() {
            return value;
        }
    }
    let path = details["path"]
        .as_str()
        .or_else(|| details["snapshot_path"].as_str());
    path.map(PathBuf::from)
        .filter(|p| p.exists())
        .map(|p| {
            if p.is_dir() {
                directory_size_bytes(&p)
            } else {
                p.metadata().map(|m| m.len()).unwrap_or(0)
            }
        })
        .unwrap_or(0)
}

fn sha256_file(path: &Path) -> HResult<String> {
    let mut file = File::open(path).map_err(|err| err.to_string())?;
    let mut digest = Sha256::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|err| err.to_string())?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn snapshot_archive_path(ctx: &Ctx, session_id: &str, snapshot_id: &str) -> PathBuf {
    ctx.snapshots_dir(session_id)
        .join(format!("{snapshot_id}.tar"))
}

fn snapshot_session_state_path(ctx: &Ctx, session_id: &str, snapshot_id: &str) -> PathBuf {
    ctx.snapshots_dir(session_id)
        .join(format!("{snapshot_id}.session.json"))
}

fn snapshot_runtime_diff_path(ctx: &Ctx, session_id: &str, snapshot_id: &str) -> PathBuf {
    ctx.snapshots_dir(session_id)
        .join(format!("{snapshot_id}.runtime-diff.tar"))
}

fn runtime_rootfs_state_path(ctx: &Ctx, session_id: &str) -> PathBuf {
    ctx.session_dir(session_id).join("runtime_rootfs.json")
}

fn read_runtime_rootfs_state(ctx: &Ctx, session_id: &str) -> Value {
    read_json(&runtime_rootfs_state_path(ctx, session_id), json!({}))
}

fn overlay_filesystem_available() -> bool {
    fs::read_to_string("/proc/filesystems")
        .map(|contents| {
            contents
                .lines()
                .any(|line| line.split_whitespace().last() == Some("overlay"))
        })
        .unwrap_or(false)
}

fn mounted_runtime_sensitive_paths(rootfs: &Path) -> Vec<String> {
    ["proc", "sys", "dev", "dev/pts"]
        .into_iter()
        .map(|relative| rootfs.join(relative))
        .filter(|path| mounted_at(path))
        .map(|path| path.display().to_string())
        .collect()
}

fn runtime_overlay_preflight(ctx: &Ctx, session_id: &str, base_rootfs: &Path) -> HResult<Value> {
    if !valid_session_id(session_id) {
        return Err(format!("invalid session_id: {session_id}"));
    }
    validate_launch_rootfs(base_rootfs)?;
    let rootfs_root = ctx.root.join("rootfs");
    if !base_rootfs.starts_with(&rootfs_root) {
        return Err(format!(
            "base rootfs must be under managed rootfs directory: {}",
            rootfs_root.display()
        ));
    }
    if !command_exists("mount") || !command_exists("umount") || !command_exists("findmnt") {
        return Err("overlay isolation requires host mount, umount, and findmnt".to_string());
    }
    if !overlay_filesystem_available() {
        return Err("kernel overlay filesystem is unavailable".to_string());
    }
    let dirty_mounts = mounted_runtime_sensitive_paths(base_rootfs);
    if !dirty_mounts.is_empty() {
        return Err(format!(
            "base rootfs has live runtime mounts and cannot be shared safely: {}",
            dirty_mounts.join(", ")
        ));
    }
    let root = ctx.runtime_rootfs_dir(session_id);
    if let Err(err) = fs::create_dir_all(&root) {
        return Err(format!(
            "runtime rootfs directory unavailable: {}: {err}",
            root.display()
        ));
    }
    let merged = root.join("merged");
    if mounted_at(&merged) {
        if session_has_live_workload(ctx, session_id) {
            return Err(format!(
                "runtime overlay is still mounted by a live workload: {}",
                merged.display()
            ));
        }
        let before = read_runtime_rootfs_state(ctx, session_id);
        for attempt in 1..=6 {
            let recovered = unmount_runtime_rootfs(ctx, session_id, false)
                .unwrap_or_else(|err| json!({"ok": false, "error": err, "attempt": attempt}));
            if !mounted_at(&merged) {
                append_runtime_event(
                    ctx,
                    session_id,
                    "runtime_rootfs_stale_mount_recovered",
                    json!({"attempt": attempt, "before": before, "after": recovered}),
                    "system",
                    "warning",
                )?;
                break;
            }
            thread::sleep(Duration::from_millis(250));
        }
        if mounted_at(&merged) {
            return Err(format!(
                "runtime overlay from a previous launch is still mounted after cleanup: {}",
                merged.display()
            ));
        }
    }
    Ok(json!({
        "schema": "gema.runtime_rootfs_overlay_preflight.v1",
        "mode": "per-session-overlay",
        "base_rootfs": base_rootfs.display().to_string(),
        "runtime_rootfs": root.display().to_string(),
        "merged": merged.display().to_string(),
        "overlay_filesystem": "available",
        "shared_rootfs_direct_mode": "disabled"
    }))
}

fn prepare_runtime_rootfs(ctx: &Ctx, session_id: &str, base_rootfs: &Path) -> HResult<Value> {
    let preflight = runtime_overlay_preflight(ctx, session_id, base_rootfs)?;
    let root = ctx.runtime_rootfs_dir(session_id);
    let merged = root.join("merged");
    let upper = root.join("upper");
    let work = root.join("work");
    fs::create_dir_all(&merged).map_err(|err| err.to_string())?;
    fs::create_dir_all(&upper).map_err(|err| err.to_string())?;
    fs::create_dir_all(&work).map_err(|err| err.to_string())?;
    if !mounted_at(&merged) {
        run_status(
            Command::new("mount")
                .arg("-t")
                .arg("overlay")
                .arg("overlay")
                .arg("-o")
                .arg(format!(
                    "lowerdir={},upperdir={},workdir={}",
                    base_rootfs.display(),
                    upper.display(),
                    work.display()
                ))
                .arg(&merged),
        )?;
    }
    fs::set_permissions(&merged, fs::Permissions::from_mode(0o755))
        .map_err(|err| err.to_string())?;
    let state = json!({"session_id": session_id, "mode": "overlay", "base_rootfs": base_rootfs.display().to_string(), "rootfs": merged.display().to_string(), "merged": merged.display().to_string(), "upper": upper.display().to_string(), "work": work.display().to_string(), "mounted": mounted_at(&merged), "preflight": preflight, "updated_at": now()});
    write_json_atomic(&runtime_rootfs_state_path(ctx, session_id), &state)?;
    append_runtime_event(
        ctx,
        session_id,
        "runtime_rootfs_prepared",
        state.clone(),
        "system",
        "info",
    )?;
    Ok(state)
}

fn unmount_runtime_rootfs(ctx: &Ctx, session_id: &str, delete: bool) -> HResult<Value> {
    let mut state = read_runtime_rootfs_state(ctx, session_id);
    if state["mode"].as_str() != Some("overlay") {
        let root = ctx.runtime_rootfs_dir(session_id);
        let merged = root.join("merged");
        if !merged.exists() && !mounted_at(&merged) {
            return Ok(state);
        }
        state = json!({
            "session_id": session_id,
            "mode": "overlay",
            "rootfs": merged.display().to_string(),
            "merged": merged.display().to_string(),
            "upper": root.join("upper").display().to_string(),
            "work": root.join("work").display().to_string(),
            "recovered_from_path": true,
            "updated_at": now()
        });
    }
    if state["mode"].as_str() != Some("overlay") {
        return Ok(state);
    }
    let merged = PathBuf::from(state["merged"].as_str().unwrap_or(""));
    for relative in ["dev/pts", "proc", "sys", "dev"] {
        let mountpoint = merged.join(relative);
        if mounted_at(&mountpoint) {
            let _ = Command::new("umount").arg(&mountpoint).status();
            if mounted_at(&mountpoint) {
                let _ = Command::new("umount").arg("-l").arg(&mountpoint).status();
            }
        }
    }
    if mounted_at(&merged) {
        let _ = Command::new("umount").arg(&merged).status();
        if mounted_at(&merged) {
            let _ = Command::new("umount").arg("-l").arg(&merged).status();
        }
    }
    state["mounted"] = json!(mounted_at(&merged));
    state["unmounted_at"] = json!(now());
    if delete {
        let root = ctx.runtime_rootfs_dir(session_id);
        if let Err(err) = fs::remove_dir_all(&root) {
            state["delete_error"] = json!(err.to_string());
        } else {
            state["deleted"] = json!(true);
        }
    }
    write_json_atomic(&runtime_rootfs_state_path(ctx, session_id), &state)?;
    append_runtime_event(
        ctx,
        session_id,
        "runtime_rootfs_unmounted",
        state.clone(),
        "system",
        "info",
    )?;
    Ok(state)
}

fn write_snapshot_session_state(ctx: &Ctx, session_id: &str, snapshot_id: &str) -> HResult<Value> {
    let session_dir = ctx.session_dir(session_id);
    let state = json!({
        "schema": "gema.snapshot.session_state.v1",
        "session_id": session_id,
        "snapshot_id": snapshot_id,
        "captured_at": now(),
        "launch": read_json(&session_dir.join("hostd.json"), json!({})),
        "substrate": read_json(&session_dir.join("meta.json"), json!({})),
        "lifecycle": read_json(&lifecycle_path(ctx, session_id), json!({})),
        "ports": read_json(&session_port_allocations_path(ctx, session_id), json!({})),
        "network": read_json(&session_dir.join("network.json"), json!({})),
        "storage_policy": read_json(&storage_policy_path(ctx, session_id), json!({})),
        "storage_volume": read_json(&storage_volume_path(ctx, session_id), json!({})),
        "gpu_assignment": read_json(&gpu_assignment_path(ctx, session_id), json!({})),
        "r2": read_r2_state(ctx, session_id),
        "agent_api": read_json(&agent_api_path(ctx, session_id), json!({})),
        "security": read_json(&session_dir.join("security.json"), json!({})),
        "processes": scan_session_processes(ctx, session_id).unwrap_or_else(|_| json!({})),
        "runtime_rootfs": read_runtime_rootfs_state(ctx, session_id),
    });
    let path = snapshot_session_state_path(ctx, session_id, snapshot_id);
    write_json_atomic(&path, &state)?;
    Ok(
        json!({"path": path.display().to_string(), "bytes": path.metadata().map(|m| m.len()).unwrap_or(0), "checksum": sha256_file(&path)?}),
    )
}

fn create_runtime_diff_snapshot(ctx: &Ctx, session_id: &str, snapshot_id: &str) -> HResult<Value> {
    let state = read_runtime_rootfs_state(ctx, session_id);
    if state["mode"].as_str() != Some("overlay") {
        return Ok(json!({"included": false, "reason": "runtime overlay disabled"}));
    }
    let upper = PathBuf::from(state["upper"].as_str().unwrap_or(""));
    if !upper.is_dir() {
        return Ok(json!({"included": false, "reason": "runtime upperdir missing"}));
    }
    let archive = snapshot_runtime_diff_path(ctx, session_id, snapshot_id);
    let out = Command::new("tar")
        .arg("-C")
        .arg(&upper)
        .arg("-cf")
        .arg(&archive)
        .arg(".")
        .output()
        .map_err(|err| err.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "runtime diff snapshot failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(
        json!({"included": true, "path": archive.display().to_string(), "bytes": archive.metadata().map(|m| m.len()).unwrap_or(0), "checksum": sha256_file(&archive)?, "runtime_rootfs": state}),
    )
}

fn restore_runtime_diff_snapshot(ctx: &Ctx, session_id: &str, snapshot: &Value) -> HResult<Value> {
    let details = &snapshot["details"];
    let runtime = &details["runtime_diff"];
    if !runtime["included"].as_bool().unwrap_or(false) {
        return Ok(json!({"restored": false, "reason": "snapshot has no runtime diff"}));
    }
    let archive = runtime["path"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            snapshot_runtime_diff_path(
                ctx,
                session_id,
                snapshot["id"].as_str().unwrap_or("snapshot"),
            )
        });
    if !archive.exists() {
        return Err(format!(
            "runtime diff archive missing: {}",
            archive.display()
        ));
    }
    if let Some(checksum) = runtime["checksum"].as_str() {
        if sha256_file(&archive)? != checksum {
            return Err("runtime diff checksum mismatch".to_string());
        }
    }
    let state = read_runtime_rootfs_state(ctx, session_id);
    if state["mode"].as_str() != Some("overlay") {
        return Err("runtime diff restore requires an overlay runtime rootfs".to_string());
    }
    let merged = PathBuf::from(state["merged"].as_str().unwrap_or(""));
    if mounted_at(&merged) {
        return Err("stop the session before restoring runtime diff".to_string());
    }
    let upper = PathBuf::from(state["upper"].as_str().unwrap_or(""));
    fs::create_dir_all(&upper).map_err(|err| err.to_string())?;
    run_status(
        Command::new("tar")
            .arg("-C")
            .arg(&upper)
            .arg("-xf")
            .arg(&archive),
    )?;
    Ok(
        json!({"restored": true, "upper": upper.display().to_string(), "archive": archive.display().to_string()}),
    )
}

fn runtime_rootfs_base_for_session(ctx: &Ctx, session_id: &str) -> HResult<PathBuf> {
    let state = read_runtime_rootfs_state(ctx, session_id);
    if let Some(base) = state["base_rootfs"]
        .as_str()
        .filter(|v| !v.trim().is_empty())
    {
        return Ok(PathBuf::from(base));
    }
    let hostd = read_json(&ctx.session_dir(session_id).join("hostd.json"), json!({}));
    if let Some(base) = hostd["base_rootfs"]
        .as_str()
        .filter(|v| !v.trim().is_empty())
    {
        return Ok(PathBuf::from(base));
    }
    let meta = read_json(&ctx.session_dir(session_id).join("meta.json"), json!({}));
    if let Some(rootfs) = meta["rootfs"].as_str().filter(|v| !v.trim().is_empty()) {
        return Ok(PathBuf::from(rootfs));
    }
    default_rootfs(ctx)
}

fn ensure_package_rootfs(ctx: &Ctx, session_id: &str) -> HResult<Value> {
    let state = read_runtime_rootfs_state(ctx, session_id);
    if state["mode"].as_str() == Some("overlay") {
        let merged = PathBuf::from(state["merged"].as_str().unwrap_or(""));
        if mounted_at(&merged) {
            return Ok(state);
        }
    }
    let base = runtime_rootfs_base_for_session(ctx, session_id)?;
    prepare_runtime_rootfs(ctx, session_id, &base)
}

fn package_manifest_path(ctx: &Ctx, session_id: &str) -> PathBuf {
    ctx.session_dir(session_id)
        .join("package-installs")
        .join("packages.jsonl")
}

fn package_history(ctx: &Ctx, session_id: &str) -> Vec<Value> {
    fs::read_to_string(package_manifest_path(ctx, session_id))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

fn parse_last_json_line(stdout: &str) -> Value {
    stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .unwrap_or_else(|| json!({}))
}

fn package_install(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let session_id = req["session_id"].as_str().ok_or("session_id required")?;
    let manager = req["manager"]
        .as_str()
        .ok_or("package manager required")?
        .to_string();
    let packages: Vec<String> = req["packages"]
        .as_array()
        .ok_or("packages required")?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    if packages.is_empty() {
        return Err("at least one package required".to_string());
    }
    let lifecycle = read_json(&lifecycle_path(ctx, session_id), json!({}));
    if matches!(
        lifecycle["state"].as_str(),
        Some("running" | "starting" | "ready")
    ) {
        return Err("stop the session before installing packages".to_string());
    }
    let rootfs_state = ensure_package_rootfs(ctx, session_id)?;
    let rootfs = rootfs_state["rootfs"]
        .as_str()
        .ok_or("runtime rootfs missing")?;
    append_runtime_event(
        ctx,
        session_id,
        "package_install_started",
        json!({"manager": manager, "packages": packages, "rootfs": rootfs}),
        req["actor"].as_str().unwrap_or("system"),
        "info",
    )?;
    let mut cmd = Command::new(ctx.package_installer());
    cmd.arg("--rootfs")
        .arg(rootfs)
        .arg("--session")
        .arg(session_id)
        .arg(&manager)
        .args(&packages);
    cmd.env("GEMA_SYSTEM_ROOT", ctx.root.display().to_string());
    let out = cmd.output().map_err(|err| err.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let result = json!({"ok": out.status.success(), "manager": manager, "packages": packages, "rootfs": rootfs, "status": out.status.code(), "stdout": stdout.chars().rev().take(4000).collect::<String>().chars().rev().collect::<String>(), "stderr": stderr.chars().rev().take(4000).collect::<String>().chars().rev().collect::<String>(), "history": package_history(ctx, session_id), "runtime_rootfs": read_runtime_rootfs_state(ctx, session_id)});
    append_runtime_event(
        ctx,
        session_id,
        if out.status.success() {
            "package_install_completed"
        } else {
            "package_install_failed"
        },
        result.clone(),
        req["actor"].as_str().unwrap_or("system"),
        if out.status.success() {
            "info"
        } else {
            "error"
        },
    )?;
    if out.status.success() {
        Ok(result)
    } else {
        Err(format!(
            "package install failed: {}",
            result["stderr"].as_str().unwrap_or("")
        ))
    }
}

fn package_list(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let session_id = req["session_id"].as_str().ok_or("session_id required")?;
    let rootfs_state = ensure_package_rootfs(ctx, session_id)?;
    let rootfs = rootfs_state["rootfs"]
        .as_str()
        .ok_or("runtime rootfs missing")?;
    let out = Command::new(ctx.package_installer())
        .arg("--rootfs")
        .arg(rootfs)
        .arg("--session")
        .arg(session_id)
        .arg("list")
        .output()
        .map_err(|err| err.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let mut parsed = parse_last_json_line(&stdout);
    parsed["ok"] = json!(out.status.success());
    parsed["runtime_rootfs"] = read_runtime_rootfs_state(ctx, session_id);
    Ok(parsed)
}

fn package_lock_action(ctx: &Ctx, req: &Value, mode: &str) -> HResult<Value> {
    let session_id = req["session_id"].as_str().ok_or("session_id required")?;
    let lifecycle = read_json(&lifecycle_path(ctx, session_id), json!({}));
    if matches!(
        lifecycle["state"].as_str(),
        Some("running" | "starting" | "ready")
    ) {
        return Err("stop the session before verifying or replaying package locks".to_string());
    }
    let rootfs_state = ensure_package_rootfs(ctx, session_id)?;
    let rootfs = rootfs_state["rootfs"]
        .as_str()
        .ok_or("runtime rootfs missing")?;
    let lock = req["lock"]
        .as_str()
        .or_else(|| req["lock_path"].as_str())
        .unwrap_or("latest");
    append_runtime_event(
        ctx,
        session_id,
        mode,
        json!({"lock": lock, "rootfs": rootfs}),
        req["actor"].as_str().unwrap_or("system"),
        "info",
    )?;
    let out = Command::new(ctx.package_installer())
        .arg("--rootfs")
        .arg(rootfs)
        .arg("--session")
        .arg(session_id)
        .arg(mode)
        .arg(lock)
        .output()
        .map_err(|err| err.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let mut parsed = parse_last_json_line(&stdout);
    parsed["ok"] = json!(out.status.success());
    parsed["status"] = json!(out.status.code());
    parsed["stderr"] = json!(stderr
        .chars()
        .rev()
        .take(4000)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>());
    parsed["runtime_rootfs"] = read_runtime_rootfs_state(ctx, session_id);
    if out.status.success() {
        Ok(parsed)
    } else {
        Err(format!(
            "package lock action failed: {}",
            parsed["stderr"].as_str().unwrap_or("")
        ))
    }
}

fn record_snapshot_size(
    ctx: &Ctx,
    session_id: &str,
    status: &str,
    details: Value,
) -> HResult<Value> {
    let mut manifest = read_snapshot_manifest(ctx, session_id);
    let snapshot_id = details["snapshot_id"]
        .as_str()
        .or_else(|| details["id"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let entry = json!({"id": snapshot_id, "status": status, "size_bytes": snapshot_size_from_details(&details), "details": details, "recorded_at": now()});
    manifest["snapshots"]
        .as_array_mut()
        .unwrap()
        .push(entry.clone());
    write_snapshot_manifest(ctx, session_id, manifest)?;
    Ok(entry)
}

fn latest_completed_snapshot(ctx: &Ctx, session_id: &str) -> Option<Value> {
    read_snapshot_manifest(ctx, session_id)["snapshots"]
        .as_array()?
        .iter()
        .rev()
        .find(|item| item["status"] == "completed")
        .cloned()
}

fn snapshot_bytes_total(ctx: &Ctx, session_id: &str) -> u64 {
    read_snapshot_manifest(ctx, session_id)["snapshots"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|item| item["status"] == "completed")
                .map(|item| item["size_bytes"].as_u64().unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}

fn collect_storage(ctx: &Ctx, session_id: &str, emit_events: bool) -> HResult<Value> {
    let policy = read_storage_policy(ctx, session_id)?;
    let workspace = session_workspace_path(ctx, session_id);
    fs::create_dir_all(&workspace).map_err(|err| err.to_string())?;
    let direct_r2_workspace = rclone_mounted_at(&workspace);
    let workspace_bytes = if direct_r2_workspace {
        0
    } else {
        directory_size_bytes(&workspace)
    };
    let snapshot_bytes = snapshot_bytes_total(ctx, session_id);
    let total = workspace_bytes + snapshot_bytes;
    let quota = policy["quota_bytes"]
        .as_u64()
        .unwrap_or(ctx.default_storage_quota_bytes);
    let exceeded = total > quota;
    let write_probe = if exceeded {
        json!({"ok": false, "error": "quota exceeded"})
    } else {
        workspace_write_probe(session_id, &workspace)
    };
    let file_count = if direct_r2_workspace {
        0
    } else {
        file_count(&workspace)
    };
    let usage = json!({"session_id": session_id, "sampled_at": now(), "workspace": workspace.display().to_string(), "quota_bytes": quota, "hard_quota_bytes": quota, "workspace_bytes": workspace_bytes, "snapshot_bytes": snapshot_bytes, "total_bytes": total, "file_count": file_count, "available_bytes": quota.saturating_sub(total), "percent_used": if quota > 0 { (total as f64 / quota as f64) * 100.0 } else { 0.0 }, "quota_exceeded": exceeded, "write_probe": write_probe, "volume": read_storage_volume(ctx, session_id), "mount": {"mounted": mounted_at(&workspace), "direct_r2": direct_r2_workspace}, "policy": policy});
    write_json_atomic(&storage_latest_path(ctx, session_id), &usage)?;
    append_jsonl(&storage_samples_path(ctx, session_id), &usage)?;
    if emit_events {
        if exceeded {
            append_runtime_event(
                ctx,
                session_id,
                "storage_quota_exceeded",
                usage.clone(),
                "system",
                "error",
            )?;
        }
        if !usage["write_probe"]["ok"].as_bool().unwrap_or(false) {
            append_runtime_event(
                ctx,
                session_id,
                "storage_write_failed",
                usage.clone(),
                "system",
                "error",
            )?;
        }
    }
    Ok(usage)
}

fn write_recovery_plan(ctx: &Ctx, session_id: &str, reason: &str) -> HResult<Value> {
    let persistence = build_persistence_status(ctx, session_id, false)
        .unwrap_or_else(|err| json!({"ok": false, "error": err}));
    let plan = json!({"session_id": session_id, "reason": reason, "latest_snapshot": latest_completed_snapshot(ctx, session_id), "latest_process_checkpoint": latest_completed_process_checkpoint(ctx, session_id), "persistence": persistence, "storage": read_json(&storage_latest_path(ctx, session_id), json!({})), "gpu_assignment": read_json(&gpu_assignment_path(ctx, session_id), json!({})), "next_actions": ["verify latest completed snapshot", "restore workspace volume if needed", "restore process checkpoint when compatible", "remount configured storage", "restart agent from durable task ledger"], "updated_at": now()});
    write_json_atomic(&recovery_plan_path(ctx, session_id), &plan)?;
    append_runtime_event(
        ctx,
        session_id,
        "recovery_plan_updated",
        plan.clone(),
        "system",
        "info",
    )?;
    Ok(plan)
}

fn create_local_snapshot(
    ctx: &Ctx,
    session_id: &str,
    snapshot_id: Option<&str>,
    actor: &str,
) -> HResult<Value> {
    let id = snapshot_id.map(str::to_string).unwrap_or_else(|| {
        format!(
            "snap-{}-{}",
            Local::now().format("%Y%m%d%H%M%S"),
            &Uuid::new_v4().to_string()[..8]
        )
    });
    let workspace = session_workspace_path(ctx, session_id);
    fs::create_dir_all(ctx.snapshots_dir(session_id)).map_err(|err| err.to_string())?;
    let archive = snapshot_archive_path(ctx, session_id, &id);
    if rclone_mounted_at(&workspace) {
        let snapshot = json!({"snapshot_id": id, "status": "skipped", "reason": "direct_r2_workspace", "workspace": workspace.display().to_string()});
        append_runtime_event(
            ctx,
            session_id,
            "snapshot_skipped",
            snapshot.clone(),
            actor,
            "info",
        )?;
        return Ok(json!({"ok": false, "skipped": true, "snapshot": snapshot}));
    }
    append_runtime_event(
        ctx,
        session_id,
        "snapshot_started",
        json!({"snapshot_id": id, "path": archive.display().to_string()}),
        actor,
        "info",
    )?;
    let out = Command::new("tar")
        .arg("-C")
        .arg(&workspace)
        .arg("-cf")
        .arg(&archive)
        .arg(".")
        .output()
        .map_err(|err| err.to_string())?;
    if !out.status.success() {
        let session_state = write_snapshot_session_state(ctx, session_id, &id).unwrap_or_else(
            |err| json!({"error": err, "path": snapshot_session_state_path(ctx, session_id, &id).display().to_string()}),
        );
        let entry = record_snapshot_size(
            ctx,
            session_id,
            "failed",
            json!({"snapshot_id": id, "path": archive.display().to_string(), "stderr": String::from_utf8_lossy(&out.stderr).to_string(), "session_state": session_state}),
        )?;
        append_runtime_event(
            ctx,
            session_id,
            "snapshot_failed",
            json!({"snapshot": entry, "stderr": String::from_utf8_lossy(&out.stderr).to_string()}),
            actor,
            "error",
        )?;
        return Ok(
            json!({"ok": false, "snapshot": entry, "stderr": String::from_utf8_lossy(&out.stderr).to_string(), "status": out.status.code()}),
        );
    }
    let process_checkpoint = if env::var("GEMA_PROCESS_CHECKPOINT_ON_SNAPSHOT")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
    {
        match process_checkpoint_create(
            ctx,
            &json!({"session_id": session_id, "checkpoint_id": format!("{id}-process"), "actor": actor}),
        ) {
            Ok(value) => value,
            Err(err) => json!({"ok": false, "error": err}),
        }
    } else {
        json!({"ok": false, "skipped": true, "reason": "GEMA_PROCESS_CHECKPOINT_ON_SNAPSHOT not enabled"})
    };
    let session_state = write_snapshot_session_state(ctx, session_id, &id)?;
    let runtime_diff = create_runtime_diff_snapshot(ctx, session_id, &id)?;
    let size = archive.metadata().map(|m| m.len()).unwrap_or(0);
    let total_size = size
        + session_state["bytes"].as_u64().unwrap_or(0)
        + runtime_diff["bytes"].as_u64().unwrap_or(0);
    let checksum = sha256_file(&archive)?;
    let entry = record_snapshot_size(
        ctx,
        session_id,
        "completed",
        json!({"snapshot_id": id, "path": archive.display().to_string(), "workspace_snapshot_bytes": size, "snapshot_bytes": total_size, "checksum": checksum, "session_state": session_state, "runtime_diff": runtime_diff, "process_checkpoint": process_checkpoint}),
    )?;
    let storage = collect_storage(ctx, session_id, true)?;
    let plan = write_recovery_plan(ctx, session_id, "snapshot_completed")?;
    let _ = build_persistence_status(ctx, session_id, false);
    append_runtime_event(
        ctx,
        session_id,
        "snapshot_completed",
        json!({"snapshot": entry, "storage": storage, "recovery_plan": plan}),
        actor,
        "info",
    )?;
    Ok(json!({"ok": true, "snapshot": entry, "storage": storage, "recovery_plan": plan}))
}

fn restore_local_snapshot(
    ctx: &Ctx,
    session_id: &str,
    snapshot_id: Option<&str>,
    actor: &str,
) -> HResult<Value> {
    let manifest = read_snapshot_manifest(ctx, session_id);
    let snapshot = manifest["snapshots"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .rev()
                .find(|item| {
                    item["status"] == "completed"
                        && snapshot_id.map_or(true, |id| item["id"].as_str() == Some(id))
                })
                .cloned()
        })
        .ok_or_else(|| "no completed snapshot found".to_string())?;
    let id = snapshot["id"]
        .as_str()
        .ok_or_else(|| "snapshot id missing".to_string())?;
    let path = snapshot["details"]["path"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| snapshot_archive_path(ctx, session_id, id));
    if !path.exists() {
        return Err(format!("snapshot archive missing: {}", path.display()));
    }
    if let Some(checksum) = snapshot["details"]["checksum"].as_str() {
        if sha256_file(&path)? != checksum {
            return Err("snapshot checksum mismatch".to_string());
        }
    }
    let workspace = session_workspace_path(ctx, session_id);
    if !rclone_mount_healthy(&workspace) {
        return Err("managed R2 workspace mount is required before snapshot restore".to_string());
    }
    run_status(
        Command::new("tar")
            .arg("-C")
            .arg(&workspace)
            .arg("-xf")
            .arg(&path),
    )?;
    let runtime_diff = restore_runtime_diff_snapshot(ctx, session_id, &snapshot)?;
    let result = json!({"snapshot": snapshot, "workspace": workspace.display().to_string(), "runtime_diff": runtime_diff, "restored_at": now()});
    append_runtime_event(
        ctx,
        session_id,
        "snapshot_restored",
        result.clone(),
        actor,
        "info",
    )?;
    write_recovery_plan(ctx, session_id, "snapshot_restored")?;
    Ok(result)
}

fn cleanup_storage(ctx: &Ctx, session_id: &str) -> HResult<Value> {
    let policy = read_storage_policy(ctx, session_id)?;
    let workspace = session_workspace_path(ctx, session_id);
    let before = collect_storage(ctx, session_id, false)?;
    let mut deleted = Vec::new();
    if policy["cleanup_policy"]["delete_tmp"]
        .as_bool()
        .unwrap_or(true)
    {
        for rel in [".gema/tmp", "tmp"] {
            let p = workspace.join(rel);
            if p.exists() {
                let size = directory_size_bytes(&p);
                let _ = fs::remove_dir_all(&p);
                deleted.push(json!({"path": p.display().to_string(), "bytes": size}));
            }
        }
    }
    if policy["cleanup_policy"]["delete_cache"]
        .as_bool()
        .unwrap_or(false)
    {
        let p = workspace.join(".cache");
        if p.exists() {
            let size = directory_size_bytes(&p);
            let _ = fs::remove_dir_all(&p);
            deleted.push(json!({"path": p.display().to_string(), "bytes": size}));
        }
    }
    let after = collect_storage(ctx, session_id, false)?;
    let result = json!({"before": before, "after": after, "deleted": deleted});
    append_runtime_event(
        ctx,
        session_id,
        "storage_cleanup_completed",
        result.clone(),
        "system",
        "info",
    )?;
    Ok(result)
}

fn r2_state_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("r2_state.json")
}

fn read_r2_state(ctx: &Ctx, session_id: &str) -> Value {
    read_json(
        &r2_state_path(ctx, session_id),
        json!({"session_id": session_id, "enabled": false}),
    )
}

fn write_r2_state(ctx: &Ctx, session_id: &str, mut state: Value) -> HResult<Value> {
    state["session_id"] = json!(session_id);
    state["updated_at"] = json!(now());
    write_json_atomic(&r2_state_path(ctx, session_id), &state)?;
    Ok(state)
}

fn configure_r2(
    ctx: &Ctx,
    session_id: &str,
    remote: Option<&str>,
    prefix: Option<&str>,
    enabled: bool,
) -> HResult<Value> {
    let remote = normalize_r2_remote(
        remote
            .map(str::to_string)
            .or_else(|| env::var("GEMA_R2_REMOTE").ok())
            .or_else(|| env::var("R2_REMOTE").ok())
            .unwrap_or_else(|| "r2:gema-cloud-volumes".to_string()),
    )?;
    let prefix = prefix
        .map(str::to_string)
        .unwrap_or_else(|| format!("sessions/{session_id}"))
        .trim_matches('/')
        .to_string();
    let mut state = read_r2_state(ctx, session_id);
    state["enabled"] = json!(enabled);
    state["remote"] = json!(remote);
    state["prefix"] = json!(prefix);
    state["workspace_remote"] = json!(format!(
        "{}/workspace",
        state["remote"]
            .as_str()
            .unwrap()
            .trim_end_matches('/')
            .to_string()
            + "/"
            + state["prefix"].as_str().unwrap()
    ));
    state["snapshot_remote"] = json!(format!(
        "{}/snapshots",
        state["remote"]
            .as_str()
            .unwrap()
            .trim_end_matches('/')
            .to_string()
            + "/"
            + state["prefix"].as_str().unwrap()
    ));
    let state = write_r2_state(ctx, session_id, state)?;
    append_runtime_event(
        ctx,
        session_id,
        "r2_configured",
        state.clone(),
        "system",
        "info",
    )?;
    Ok(state)
}

fn normalize_r2_remote(remote: String) -> HResult<String> {
    let remote = remote.trim().trim_end_matches('/').to_string();
    if remote.is_empty() {
        return Err("r2 remote required".to_string());
    }
    let Some((_name, path)) = remote.split_once(':') else {
        return Err(format!(
            "r2 remote must be a bucket-qualified rclone path like {remote}:<bucket>"
        ));
    };
    if path.trim_matches('/').is_empty() {
        return Err(
            "r2 remote must include a bucket after the remote name, for example r2:<bucket>"
                .to_string(),
        );
    }
    Ok(remote)
}

fn r2_probe(ctx: &Ctx, session_id: &str) -> HResult<Value> {
    let state = read_r2_state(ctx, session_id);
    if !state["enabled"].as_bool().unwrap_or(false) {
        return Ok(json!({"ok": false, "state": state, "error": "r2 not enabled"}));
    }
    if !command_exists("rclone") {
        return Ok(json!({"ok": false, "state": state, "error": "rclone missing"}));
    }
    let remote = state["workspace_remote"].as_str().unwrap_or("");
    let rclone = rclone_bin();
    let proc = command_output(&[rclone.as_str(), "lsf", remote, "--max-depth", "1"]);
    let stderr = proc["stderr"].as_str().unwrap_or("");
    let cold_prefix = stderr.contains("directory not found");
    let result = json!({
        "ok": proc["ok"].as_bool().unwrap_or(false) || cold_prefix,
        "cold_prefix": cold_prefix,
        "state": state,
        "stdout": proc["stdout"].clone(),
        "stderr": proc["stderr"].clone(),
        "status": proc["status"].clone()
    });
    append_runtime_event(
        ctx,
        session_id,
        "r2_probe",
        result.clone(),
        "system",
        if result["ok"].as_bool().unwrap_or(false) {
            "info"
        } else {
            "error"
        },
    )?;
    Ok(result)
}

fn r2_sync(ctx: &Ctx, session_id: &str, target: &str, direction: &str) -> HResult<Value> {
    let mut state = read_r2_state(ctx, session_id);
    if !state["enabled"].as_bool().unwrap_or(false) {
        return Err("r2 not configured".to_string());
    }
    if !command_exists("rclone") {
        return Err("rclone missing".to_string());
    }
    let local = if target == "snapshots" {
        ctx.snapshots_dir(session_id)
    } else {
        session_workspace_path(ctx, session_id)
    };
    let remote = if target == "snapshots" {
        state["snapshot_remote"].as_str().unwrap_or("")
    } else {
        state["workspace_remote"].as_str().unwrap_or("")
    };
    fs::create_dir_all(&local).map_err(|err| err.to_string())?;
    let snapshot_manifest_copy = local.join("snapshots.json");
    if target == "snapshots" && direction != "pull" {
        let manifest = read_snapshot_manifest(ctx, session_id);
        write_json_atomic(&snapshot_manifest_copy, &manifest)?;
        let recovery_plan = read_json(&recovery_plan_path(ctx, session_id), Value::Null);
        if recovery_plan.is_object() {
            write_json_atomic(&local.join("recovery_plan.json"), &recovery_plan)?;
        }
    }
    let local_string = local.display().to_string();
    let rclone = rclone_bin();
    let argv = if direction == "pull" {
        vec![
            rclone.as_str(),
            "copy",
            remote,
            local_string.as_str(),
            "--checksum",
            "--fast-list",
        ]
    } else {
        vec![
            rclone.as_str(),
            "copy",
            local_string.as_str(),
            remote,
            "--checksum",
            "--fast-list",
        ]
    };
    append_runtime_event(
        ctx,
        session_id,
        "r2_sync_started",
        json!({"target": target, "direction": direction, "local": local.display().to_string(), "remote": remote}),
        "system",
        "info",
    )?;
    let out = command_output(&argv);
    if target == "snapshots" && direction == "pull" && snapshot_manifest_copy.exists() {
        let manifest = read_json(&snapshot_manifest_copy, Value::Null);
        if manifest.is_object() {
            write_snapshot_manifest(ctx, session_id, manifest)?;
        }
        let pulled_recovery = local.join("recovery_plan.json");
        if pulled_recovery.exists() {
            let recovery = read_json(&pulled_recovery, Value::Null);
            if recovery.is_object() {
                write_json_atomic(&recovery_plan_path(ctx, session_id), &recovery)?;
            }
        }
    }
    let result = json!({"ok": out["ok"].clone(), "target": target, "direction": direction, "local": local.display().to_string(), "remote": remote, "started_at": now(), "finished_at": now(), "status": out["status"].clone(), "stdout": out["stdout"].as_str().unwrap_or("").chars().rev().take(4000).collect::<String>().chars().rev().collect::<String>(), "stderr": out["stderr"].as_str().unwrap_or("").chars().rev().take(4000).collect::<String>().chars().rev().collect::<String>(), "storage": collect_storage(ctx, session_id, true)?});
    state["last_sync"] = result.clone();
    write_r2_state(ctx, session_id, state)?;
    append_runtime_event(
        ctx,
        session_id,
        if result["ok"].as_bool().unwrap_or(false) {
            "r2_sync_finished"
        } else {
            "r2_sync_failed"
        },
        result.clone(),
        "system",
        if result["ok"].as_bool().unwrap_or(false) {
            "info"
        } else {
            "error"
        },
    )?;
    Ok(result)
}

fn r2_mountpoint(ctx: &Ctx, session_id: &str, target: &str) -> PathBuf {
    if target == "workspace" {
        return session_workspace_path(ctx, session_id);
    }
    ctx.root.join("r2-mounts").join(session_id).join(target)
}

fn r2_mount_unit_name(session_id: &str, target: &str) -> String {
    let safe_session: String = session_id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' { ch } else { '-' })
        .collect();
    let safe_target: String = target
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' { ch } else { '-' })
        .collect();
    format!("gema-r2-mount-{safe_session}-{safe_target}.service")
}

fn session_guest_uid(session_id: &str) -> u32 {
    let hex: String = session_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(8)
        .collect();
    let seed = if hex.is_empty() {
        session_id
            .bytes()
            .fold(0_u32, |acc, byte| acc.wrapping_mul(16777619) ^ byte as u32)
    } else {
        u32::from_str_radix(&hex, 16).unwrap_or(0)
    };
    200_000 + (seed % 400_000)
}

fn session_guest_gid(session_id: &str) -> u32 {
    session_guest_uid(session_id)
}

fn r2_mount(ctx: &Ctx, session_id: &str, target: &str) -> HResult<Value> {
    let mut state = read_r2_state(ctx, session_id);
    if !state["enabled"].as_bool().unwrap_or(false) {
        return Err("r2 not configured".to_string());
    }
    if !command_exists("rclone") {
        return Err("rclone missing".to_string());
    }
    let remote = if target == "snapshots" {
        state["snapshot_remote"].as_str().unwrap_or("")
    } else {
        state["workspace_remote"].as_str().unwrap_or("")
    };
    let mountpoint = r2_mountpoint(ctx, session_id, target);
    prepare_r2_mountpoint(&mountpoint)?;
    if rclone_mounted_at(&mountpoint) {
        if rclone_mount_healthy(&mountpoint) {
            let result = json!({"ok": true, "target": target, "remote": remote, "mountpoint": mountpoint.display().to_string(), "mounted": true, "healthy": true, "already_mounted": true, "unit": r2_mount_unit_name(session_id, target), "fstype": mounted_fstype(&mountpoint)});
            state["mounts"][target] = result.clone();
            write_r2_state(ctx, session_id, state)?;
            return Ok(result);
        }
        append_runtime_event(
            ctx,
            session_id,
            "r2_mount_stale_detected",
            json!({"target": target, "remote": remote, "mountpoint": mountpoint.display().to_string(), "fstype": mounted_fstype(&mountpoint)}),
            "system",
            "warning",
        )?;
        let _ = r2_unmount(ctx, session_id, target);
    }
    let log_path = ctx
        .session_dir(session_id)
        .join("logs")
        .join(format!("r2-mount-{target}.log"));
    fs::create_dir_all(log_path.parent().unwrap()).map_err(|err| err.to_string())?;
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|err| err.to_string())?;
    let cache_root = env::var("GEMA_RCLONE_CACHE_ROOT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "/var/lib/gema-system/rclone-cache".to_string());
    let cache_dir = PathBuf::from(cache_root)
        .join(session_id)
        .join(target);
    let guest_uid = session_guest_uid(session_id).to_string();
    let guest_gid = session_guest_gid(session_id).to_string();
    fs::create_dir_all(&cache_dir).map_err(|err| err.to_string())?;
    append_runtime_event(
        ctx,
        session_id,
        "r2_mount_started",
        json!({"target": target, "remote": remote, "mountpoint": mountpoint.display().to_string(), "cache_dir": cache_dir.display().to_string()}),
        "system",
        "info",
    )?;
    let unit = r2_mount_unit_name(session_id, target);
    let rclone_args = vec![
        "mount".to_string(),
        remote.to_string(),
        mountpoint.display().to_string(),
        "--vfs-cache-mode".to_string(),
        "full".to_string(),
        "--vfs-cache-max-size".to_string(),
        env::var("RCLONE_VFS_CACHE_SIZE").unwrap_or_else(|_| "12G".to_string()),
        "--vfs-cache-max-age".to_string(),
        env::var("RCLONE_VFS_CACHE_MAX_AGE").unwrap_or_else(|_| "6h".to_string()),
        "--vfs-write-back".to_string(),
        "5s".to_string(),
        "--dir-cache-time".to_string(),
        "30s".to_string(),
        "--attr-timeout".to_string(),
        "1s".to_string(),
        "--poll-interval".to_string(),
        "0".to_string(),
        "--umask".to_string(),
        "007".to_string(),
        "--file-perms".to_string(),
        "0770".to_string(),
        "--dir-perms".to_string(),
        "0770".to_string(),
        "--uid".to_string(),
        guest_uid,
        "--gid".to_string(),
        guest_gid,
        "--allow-other".to_string(),
        "--links".to_string(),
        "--cache-dir".to_string(),
        cache_dir.display().to_string(),
        "--log-file".to_string(),
        log_path.display().to_string(),
        "--log-level".to_string(),
        "INFO".to_string(),
    ];
    let child_id = if command_exists("systemd-run") {
        let _ = Command::new("systemctl").args(["stop", &unit]).status();
        let _ = Command::new("systemctl").args(["reset-failed", &unit]).status();
        let mut cmd = Command::new("systemd-run");
        cmd.arg("--unit")
            .arg(&unit)
            .arg("--collect")
            .arg("--property")
            .arg("Restart=on-failure")
            .arg("--property")
            .arg("RestartSec=3")
            .arg("--property")
            .arg("KillMode=mixed")
            .arg("--property")
            .arg("Type=simple")
            .arg(rclone_bin())
            .args(&rclone_args);
        run_status(&mut cmd)?;
        0
    } else {
        let child = Command::new(rclone_bin())
            .args(&rclone_args)
            .stdout(log_file.try_clone().map_err(|err| err.to_string())?)
            .stderr(log_file)
            .spawn()
            .map_err(|err| err.to_string())?;
        child.id()
    };
    let mut mounted = false;
    for _ in 0..100 {
        if rclone_mount_healthy(&mountpoint) {
            mounted = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if !mounted {
        let _ = Command::new("systemctl").args(["stop", &unit]).status();
        lock_r2_mountpoint(&mountpoint);
    }
    let result = json!({"ok": mounted, "target": target, "remote": remote, "mountpoint": mountpoint.display().to_string(), "pid": child_id, "unit": unit, "mounted": mounted, "healthy": mounted, "fstype": mounted_fstype(&mountpoint), "log_path": log_path.display().to_string()});
    state["mounts"][target] = result.clone();
    write_r2_state(ctx, session_id, state)?;
    append_runtime_event(
        ctx,
        session_id,
        if mounted {
            "r2_mount_completed"
        } else {
            "r2_mount_failed"
        },
        result.clone(),
        "system",
        if mounted { "info" } else { "error" },
    )?;
    Ok(result)
}

fn r2_unmount(ctx: &Ctx, session_id: &str, target: &str) -> HResult<Value> {
    let mut state = read_r2_state(ctx, session_id);
    let mountpoint = r2_mountpoint(ctx, session_id, target);
    let unit = r2_mount_unit_name(session_id, target);
    let _ = Command::new("systemctl").args(["stop", &unit]).status();
    detach_stale_rclone_mount(&mountpoint);
    let before = rclone_mounted_at(&mountpoint);
    let mut status = 0;
    let mut stderr = String::new();
    if before {
        let out = if command_exists("fusermount3") {
            Command::new("fusermount3")
                .arg("-uz")
                .arg(&mountpoint)
                .output()
        } else if command_exists("fusermount") {
            Command::new("fusermount")
                .arg("-uz")
                .arg(&mountpoint)
                .output()
        } else {
            Command::new("umount").arg(&mountpoint).output()
        };
        if let Ok(out) = out {
            status = out.status.code().unwrap_or(-1);
            stderr = String::from_utf8_lossy(&out.stderr).to_string();
            if !out.status.success() {
                let _ = Command::new("umount").arg("-l").arg(&mountpoint).status();
            }
        }
        for _ in 0..20 {
            if !rclone_mounted_at(&mountpoint) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        for _ in 0..6 {
            if !rclone_mounted_at(&mountpoint) {
                break;
            }
            detach_mount_lazy(&mountpoint);
            thread::sleep(Duration::from_millis(250));
        }
    }
    let mounted = rclone_mounted_at(&mountpoint);
    let healthy = mounted && rclone_mount_healthy(&mountpoint);
    if !mounted {
        lock_r2_mountpoint(&mountpoint);
    }
    let result = json!({"ok": !mounted, "target": target, "mountpoint": mountpoint.display().to_string(), "unit": unit, "was_mounted": before, "mounted": mounted, "healthy": healthy, "fstype": mounted_fstype(&mountpoint), "status": status, "stderr": stderr});
    state["mounts"][target] = result.clone();
    write_r2_state(ctx, session_id, state)?;
    append_runtime_event(
        ctx,
        session_id,
        if result["ok"].as_bool().unwrap_or(false) {
            "r2_unmounted"
        } else {
            "r2_unmount_failed"
        },
        result.clone(),
        "system",
        if result["ok"].as_bool().unwrap_or(false) {
            "info"
        } else {
            "error"
        },
    )?;
    Ok(result)
}

fn user_s3_state_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("user_s3_state.json")
}

fn user_s3_rclone_config_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("user_s3.rclone.conf")
}

fn checkpoint_policy_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("checkpoint_policy.json")
}

fn persistence_status_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("persistence_status.json")
}

fn read_user_s3_state(ctx: &Ctx, session_id: &str) -> Value {
    read_json(
        &user_s3_state_path(ctx, session_id),
        json!({"session_id": session_id, "enabled": false}),
    )
}

fn write_user_s3_state(ctx: &Ctx, session_id: &str, mut state: Value) -> HResult<Value> {
    state["session_id"] = json!(session_id);
    state["updated_at"] = json!(now());
    let path = user_s3_state_path(ctx, session_id);
    write_json_atomic(&path, &state)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).ok();
    Ok(state)
}

fn user_s3_prefix(prefix: Option<&str>) -> String {
    prefix.unwrap_or("").trim().trim_matches('/').to_string()
}

fn user_s3_remote_root(state: &Value) -> HResult<String> {
    let bucket = state["bucket"]
        .as_str()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| "user s3 bucket required".to_string())?;
    let prefix = state["prefix"].as_str().unwrap_or("").trim_matches('/');
    Ok(if prefix.is_empty() {
        format!("user_s3:{bucket}")
    } else {
        format!("user_s3:{bucket}/{prefix}")
    })
}

fn write_user_s3_rclone_config(ctx: &Ctx, session_id: &str, state: &Value) -> HResult<PathBuf> {
    let access_key = state["access_key"]
        .as_str()
        .ok_or_else(|| "user s3 access key required".to_string())?;
    let secret_key = state["secret_key"]
        .as_str()
        .ok_or_else(|| "user s3 secret key required".to_string())?;
    let endpoint = state["endpoint"].as_str().unwrap_or("").trim();
    let region = state["region"].as_str().unwrap_or("us-east-1").trim();
    let force_path_style = state["force_path_style"].as_bool().unwrap_or(true);
    let provider = state["provider"].as_str().unwrap_or("Other");
    let mut config = format!(
        "[user_s3]\ntype = s3\nprovider = {provider}\naccess_key_id = {access_key}\nsecret_access_key = {secret_key}\nregion = {region}\nforce_path_style = {}\n",
        if force_path_style { "true" } else { "false" }
    );
    if !endpoint.is_empty() {
        config.push_str(&format!("endpoint = {endpoint}\n"));
    }
    let path = user_s3_rclone_config_path(ctx, session_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(&path, config).map_err(|err| err.to_string())?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).ok();
    Ok(path)
}

fn configure_user_s3(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let session_id = req["session_id"].as_str().ok_or("session_id required")?;
    let enabled = req["enabled"].as_bool().unwrap_or(true);
    let state = json!({
        "enabled": enabled,
        "provider": req["provider"].as_str().unwrap_or("Other"),
        "endpoint": req["endpoint"].as_str().unwrap_or(""),
        "region": req["region"].as_str().unwrap_or("us-east-1"),
        "bucket": req["bucket"].as_str().ok_or("bucket required")?,
        "prefix": user_s3_prefix(req["prefix"].as_str()),
        "access_key": req["access_key"].as_str().ok_or("access_key required")?,
        "secret_key": req["secret_key"].as_str().ok_or("secret_key required")?,
        "force_path_style": req["force_path_style"].as_bool().unwrap_or(true),
    });
    let config_path = write_user_s3_rclone_config(ctx, session_id, &state)?;
    let remote_root = user_s3_remote_root(&state)?;
    let mut public = state.clone();
    public["access_key"] = json!("stored");
    public["secret_key"] = json!("stored");
    public["rclone_config"] = json!(config_path.display().to_string());
    public["workspace_remote"] = json!(format!("{remote_root}/workspace"));
    public["snapshot_remote"] = json!(format!("{remote_root}/snapshots"));
    let stored = write_user_s3_state(ctx, session_id, public)?;
    append_runtime_event(
        ctx,
        session_id,
        "user_s3_configured",
        stored.clone(),
        "system",
        "info",
    )?;
    Ok(stored)
}

fn user_s3_remote(ctx: &Ctx, session_id: &str, target: &str) -> HResult<(PathBuf, String)> {
    let state = read_user_s3_state(ctx, session_id);
    if !state["enabled"].as_bool().unwrap_or(false) {
        return Err("user s3 not configured".to_string());
    }
    let config_path = user_s3_rclone_config_path(ctx, session_id);
    if !config_path.exists() {
        return Err("user s3 rclone config missing".to_string());
    }
    let remote = if target == "snapshots" {
        state["snapshot_remote"].as_str().unwrap_or("").to_string()
    } else {
        state["workspace_remote"].as_str().unwrap_or("").to_string()
    };
    if remote.is_empty() {
        return Err("user s3 remote missing".to_string());
    }
    Ok((config_path, remote))
}

fn user_s3_probe(ctx: &Ctx, session_id: &str) -> HResult<Value> {
    let state = read_user_s3_state(ctx, session_id);
    if !state["enabled"].as_bool().unwrap_or(false) {
        return Ok(json!({"ok": false, "state": state, "error": "user s3 not enabled"}));
    }
    if !command_exists("rclone") {
        return Ok(json!({"ok": false, "state": state, "error": "rclone missing"}));
    }
    let (config_path, remote) = user_s3_remote(ctx, session_id, "workspace")?;
    let config = config_path.display().to_string();
    let out = command_output(&[
        "rclone",
        "--config",
        &config,
        "lsf",
        &remote,
        "--max-depth",
        "1",
    ]);
    let stderr = out["stderr"].as_str().unwrap_or("");
    let cold_prefix = stderr.contains("directory not found") || stderr.contains("not found");
    let result = json!({"ok": out["ok"].as_bool().unwrap_or(false) || cold_prefix, "cold_prefix": cold_prefix, "state": state, "stdout": out["stdout"], "stderr": out["stderr"], "status": out["status"]});
    append_runtime_event(
        ctx,
        session_id,
        "user_s3_probe",
        result.clone(),
        "system",
        if result["ok"].as_bool().unwrap_or(false) {
            "info"
        } else {
            "error"
        },
    )?;
    Ok(result)
}

fn user_s3_sync(ctx: &Ctx, session_id: &str, target: &str, direction: &str) -> HResult<Value> {
    if !command_exists("rclone") {
        return Err("rclone missing".to_string());
    }
    let (config_path, remote) = user_s3_remote(ctx, session_id, target)?;
    let local = if target == "snapshots" {
        ctx.snapshots_dir(session_id)
    } else {
        session_workspace_path(ctx, session_id)
    };
    fs::create_dir_all(&local).map_err(|err| err.to_string())?;
    if target == "snapshots" && direction != "pull" {
        write_json_atomic(
            &local.join("snapshots.json"),
            &read_snapshot_manifest(ctx, session_id),
        )?;
        let recovery_plan = read_json(&recovery_plan_path(ctx, session_id), Value::Null);
        if recovery_plan.is_object() {
            write_json_atomic(&local.join("recovery_plan.json"), &recovery_plan)?;
        }
    }
    let local_string = local.display().to_string();
    let config = config_path.display().to_string();
    let argv = if direction == "pull" {
        vec![
            "rclone",
            "--config",
            config.as_str(),
            "copy",
            remote.as_str(),
            local_string.as_str(),
            "--checksum",
            "--fast-list",
        ]
    } else {
        vec![
            "rclone",
            "--config",
            config.as_str(),
            "copy",
            local_string.as_str(),
            remote.as_str(),
            "--checksum",
            "--fast-list",
        ]
    };
    append_runtime_event(
        ctx,
        session_id,
        "user_s3_sync_started",
        json!({"target": target, "direction": direction, "local": local_string, "remote": remote}),
        "system",
        "info",
    )?;
    let out = command_output(&argv);
    if target == "snapshots" && direction == "pull" {
        let manifest_copy = local.join("snapshots.json");
        if manifest_copy.exists() {
            let manifest = read_json(&manifest_copy, Value::Null);
            if manifest.is_object() {
                write_snapshot_manifest(ctx, session_id, manifest)?;
            }
        }
    }
    let result = json!({"ok": out["ok"], "target": target, "direction": direction, "local": local.display().to_string(), "remote": remote, "status": out["status"], "stdout": out["stdout"], "stderr": out["stderr"], "finished_at": now()});
    append_runtime_event(
        ctx,
        session_id,
        if result["ok"].as_bool().unwrap_or(false) {
            "user_s3_sync_finished"
        } else {
            "user_s3_sync_failed"
        },
        result.clone(),
        "system",
        if result["ok"].as_bool().unwrap_or(false) {
            "info"
        } else {
            "error"
        },
    )?;
    Ok(result)
}

fn user_s3_mountpoint(ctx: &Ctx, session_id: &str, target: &str) -> PathBuf {
    ctx.root
        .join("user-s3-mounts")
        .join(session_id)
        .join(target)
}

fn user_s3_mount(ctx: &Ctx, session_id: &str, target: &str) -> HResult<Value> {
    if !command_exists("rclone") {
        return Err("rclone missing".to_string());
    }
    let (config_path, remote) = user_s3_remote(ctx, session_id, target)?;
    let mountpoint = user_s3_mountpoint(ctx, session_id, target);
    fs::create_dir_all(&mountpoint).map_err(|err| err.to_string())?;
    if mounted_at(&mountpoint) {
        return Ok(
            json!({"ok": true, "target": target, "remote": remote, "mountpoint": mountpoint.display().to_string(), "mounted": true, "already_mounted": true}),
        );
    }
    let log_path = ctx
        .session_dir(session_id)
        .join("logs")
        .join(format!("user-s3-mount-{target}.log"));
    fs::create_dir_all(log_path.parent().unwrap()).map_err(|err| err.to_string())?;
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|err| err.to_string())?;
    let config = config_path.display().to_string();
    append_runtime_event(
        ctx,
        session_id,
        "user_s3_mount_started",
        json!({"target": target, "remote": remote, "mountpoint": mountpoint.display().to_string()}),
        "system",
        "info",
    )?;
    let guest_uid = session_guest_uid(session_id).to_string();
    let guest_gid = session_guest_gid(session_id).to_string();
    let mut child = Command::new("rclone")
        .args(["--config", config.as_str(), "mount", remote.as_str()])
        .arg(&mountpoint)
        .args([
            "--vfs-cache-mode",
            "writes",
            "--dir-cache-time",
            "30s",
            "--poll-interval",
            "15s",
            "--umask",
            "007",
            "--file-perms",
            "0770",
            "--dir-perms",
            "0770",
            "--uid",
            guest_uid.as_str(),
            "--gid",
            guest_gid.as_str(),
            "--links",
        ])
        .stdout(log_file.try_clone().map_err(|err| err.to_string())?)
        .stderr(log_file)
        .spawn()
        .map_err(|err| err.to_string())?;
    let mut mounted = false;
    for _ in 0..100 {
        if mounted_at(&mountpoint) {
            mounted = true;
            break;
        }
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if !mounted {
        let _ = child.kill();
    }
    let result = json!({"ok": mounted, "target": target, "remote": remote, "mountpoint": mountpoint.display().to_string(), "pid": child.id(), "mounted": mounted, "log_path": log_path.display().to_string()});
    append_runtime_event(
        ctx,
        session_id,
        if mounted {
            "user_s3_mount_completed"
        } else {
            "user_s3_mount_failed"
        },
        result.clone(),
        "system",
        if mounted { "info" } else { "error" },
    )?;
    Ok(result)
}

fn user_s3_unmount(ctx: &Ctx, session_id: &str, target: &str) -> HResult<Value> {
    let mountpoint = user_s3_mountpoint(ctx, session_id, target);
    let before = mounted_at(&mountpoint);
    if before {
        let out = if command_exists("fusermount3") {
            Command::new("fusermount3")
                .arg("-u")
                .arg(&mountpoint)
                .output()
        } else if command_exists("fusermount") {
            Command::new("fusermount")
                .arg("-u")
                .arg(&mountpoint)
                .output()
        } else {
            Command::new("umount").arg(&mountpoint).output()
        };
        if out.as_ref().map(|v| !v.status.success()).unwrap_or(true) {
            let _ = Command::new("umount").arg("-l").arg(&mountpoint).status();
        }
    }
    let result = json!({"ok": !mounted_at(&mountpoint), "target": target, "mountpoint": mountpoint.display().to_string(), "was_mounted": before, "mounted": mounted_at(&mountpoint)});
    append_runtime_event(
        ctx,
        session_id,
        "user_s3_unmounted",
        result.clone(),
        "system",
        "info",
    )?;
    Ok(result)
}

fn read_checkpoint_policy(ctx: &Ctx, session_id: &str) -> Value {
    read_json(
        &checkpoint_policy_path(ctx, session_id),
        json!({
            "schema": "gema.persistence_policy.v1",
            "session_id": session_id,
            "enabled": true,
            "backend": "managed",
            "workspace_policy": "managed",
            "checkpoint_target": "managed",
            "interval_seconds": 300,
            "restore_policy": "latest-consistent",
            "retention_policy": "rolling",
            "process_checkpoint": {
                "enabled": env::var("GEMA_PROCESS_CHECKPOINT_ON_SNAPSHOT")
                    .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
                    .unwrap_or(false),
                "mode": "best-effort"
            },
            "guarantees": {
                "workspace_files": "checkpointed",
                "runtime_diff": "checkpointed_when_runtime_rootfs_exists",
                "process_memory": "best_effort_when_enabled",
                "open_network_connections": "not_guaranteed"
            },
            "last_snapshot_at": null,
            "last_snapshot_id": null,
            "last_verified_restore_point": null,
            "created_at": now()
        }),
    )
}

fn write_checkpoint_policy(ctx: &Ctx, session_id: &str, mut policy: Value) -> HResult<Value> {
    policy["schema"] = json!("gema.persistence_policy.v1");
    policy["session_id"] = json!(session_id);
    policy["updated_at"] = json!(now());
    write_json_atomic(&checkpoint_policy_path(ctx, session_id), &policy)?;
    Ok(policy)
}

fn rootfs_manifest(rootfs: &Path) -> Value {
    read_json(&rootfs.join("etc/gema-image.json"), Value::Null)
}

fn rootfs_is_ml_profile(rootfs: &Path) -> bool {
    let manifest = rootfs_manifest(rootfs);
    manifest["profile"].as_str() == Some("gpu-ml")
        || manifest["capabilities"]["ml"]["lanes"]
            .as_object()
            .is_some_and(|lanes| !lanes.is_empty())
}

fn rootfs_has_ml_bridge(rootfs: &Path) -> bool {
    rootfs.join("opt/gema/bin/gema-fabric-bridge").exists()
        || rootfs.join("opt/gema/bin/gema-ml-bridge").exists()
}

fn ml_bridge_command(command: &[Value]) -> Vec<Value> {
    let mut wrapped = vec![
        json!("/opt/gema/bin/gema-ml-bridge"),
        json!("run"),
        json!("--"),
    ];
    wrapped.extend(command.iter().cloned());
    wrapped
}

fn command_looks_expensive_training(command: &[Value]) -> bool {
    let joined = command
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    [
        "train",
        "trainer.fit",
        "accelerate launch",
        "torchrun",
        "deepspeed",
        "finetune",
        "fine-tune",
        "sft",
        "pretrain",
        "ray train",
    ]
    .iter()
    .any(|needle| joined.contains(needle))
}

fn ensure_ml_recovery_contract(
    ctx: &Ctx,
    session_id: &str,
    workspace: &Path,
    rootfs: &Path,
    command: &[Value],
) -> HResult<Value> {
    fs::create_dir_all(workspace.join(".gema/recovery")).map_err(|err| err.to_string())?;
    fs::create_dir_all(workspace.join(".gema/checkpoints")).map_err(|err| err.to_string())?;
    fs::create_dir_all(workspace.join(".gema/logs")).map_err(|err| err.to_string())?;

    let checkpoint_policy = {
        let mut policy = read_checkpoint_policy(ctx, session_id);
        policy["enabled"] = json!(true);
        policy["backend"] = json!(policy["backend"].as_str().unwrap_or("managed"));
        policy["checkpoint_target"] =
            json!(policy["checkpoint_target"].as_str().unwrap_or("managed"));
        policy["workspace_policy"] =
            json!(policy["workspace_policy"].as_str().unwrap_or("managed"));
        policy["interval_seconds"] = json!(policy["interval_seconds"]
            .as_u64()
            .unwrap_or(300)
            .clamp(60, 3600));
        policy["restore_policy"] = json!("latest-consistent");
        policy["retention_policy"] =
            json!(policy["retention_policy"].as_str().unwrap_or("rolling"));
        policy["framework_checkpoint"] = json!({
            "required_before_expensive_training": true,
            "root": "/home/gema/workspace/.gema/checkpoints",
            "local_root": workspace.join(".gema/checkpoints").display().to_string(),
            "cadence": {
                "save_every_seconds": policy["interval_seconds"].as_u64().unwrap_or(300).clamp(60, 3600),
                "save_on_epoch_end": true,
                "save_on_signal": ["SIGTERM", "SIGINT"]
            },
            "frameworks": {
                "torch": {"state_dict_path": ".gema/checkpoints/torch/latest.pt"},
                "transformers": {"output_dir": ".gema/checkpoints/transformers", "resume_from_checkpoint": true},
                "accelerate": {"project_dir": ".gema/checkpoints/accelerate", "automatic_checkpoint_naming": true},
                "lightning": {"dirpath": ".gema/checkpoints/lightning", "save_last": true},
                "deepspeed": {"save_dir": ".gema/checkpoints/deepspeed"},
                "ray": {"storage_path": ".gema/checkpoints/ray"}
            }
        });
        write_checkpoint_policy(ctx, session_id, policy)?
    };

    let manifest = rootfs_manifest(rootfs);
    let expensive_training = command_looks_expensive_training(command);
    let resume = json!({
        "schema": "gema.ml_resume.v1",
        "session_id": session_id,
        "created_at": now(),
        "updated_at": now(),
        "mandatory": true,
        "expensive_training_gate": {
            "enabled": true,
            "command_detected_as_training": expensive_training,
            "allowed": true,
            "reason": "recovery resume contract and framework checkpoint policy are present"
        },
        "workspace": {
            "root": "/home/gema/workspace",
            "host_path": workspace.display().to_string(),
            "recovery_dir": ".gema/recovery",
            "checkpoint_dir": ".gema/checkpoints",
            "log": ".gema/logs/workload.log"
        },
        "image": {
            "id": manifest["id"].clone(),
            "profile": manifest["profile"].clone(),
            "checksum": manifest["checksum"].clone(),
            "ml": manifest["capabilities"]["ml"].clone(),
            "cuda": manifest["compatibility"]["requires"]["cuda"].clone()
        },
        "command": command,
        "hostd": {
            "checkpoint_policy": checkpoint_policy_path(ctx, session_id).display().to_string(),
            "recovery_plan": recovery_plan_path(ctx, session_id).display().to_string()
        },
        "resume_order": [
            "read .gema/recovery/resume.json",
            "restore latest verified Gema snapshot when available",
            "restore framework checkpoint from .gema/checkpoints",
            "fall back to application-specific resume arguments"
        ],
        "not_guaranteed": [
            "GPU VRAM",
            "remote peer state",
            "unsynced framework buffers",
            "non-workspace files unless captured by runtime diff"
        ]
    });
    write_json_atomic(
        &workspace.join(".gema/recovery/framework-checkpoint-policy.json"),
        &checkpoint_policy,
    )?;
    write_json_atomic(&workspace.join(".gema/recovery/resume.json"), &resume)?;
    append_runtime_event(
        ctx,
        session_id,
        "ml_recovery_contract_created",
        json!({"resume": workspace.join(".gema/recovery/resume.json").display().to_string(), "framework_policy": workspace.join(".gema/recovery/framework-checkpoint-policy.json").display().to_string(), "expensive_training_detected": expensive_training}),
        "system",
        "info",
    )?;
    Ok(json!({"resume": resume, "checkpoint_policy": checkpoint_policy}))
}

fn execution_contract_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("execution_contract.json")
}

fn accelerator_state_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("accelerator_state.json")
}

fn fabric_manifest_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("fabric_manifest.json")
}

fn cuda_proxy_state_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("cuda_proxy_state.json")
}

fn fabric_workspace_dir(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.workspace_dir(id).join(".gema").join("fabric")
}

fn fabric_agent_dir(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.workspace_dir(id)
        .join(".gema")
        .join("agent")
        .join("fabric")
}

fn write_fabric_mirrors(ctx: &Ctx, id: &str, name: &str, payload: &Value) -> HResult<()> {
    write_json_atomic(&fabric_workspace_dir(ctx, id).join(name), payload)?;
    write_json_atomic(&fabric_agent_dir(ctx, id).join(name), payload)?;
    Ok(())
}

fn gvphase1_host_source_status() -> Value {
    let root = Path::new("/root/gvphase1");
    let build = root.join("build");
    let libs = [
        "libcuda.so.1",
        "libcudart.so",
        "libcublas.so",
        "libcublasLt.so",
        "libcudnn.so",
        "libcurand.so",
        "libcufft.so",
        "libcusparse.so",
        "libcusolver.so",
        "libnvrtc.so",
        "libnvidia-ml.so",
    ];
    let mut lib_status = Map::new();
    for name in libs {
        lib_status.insert(
            name.to_string(),
            json!(build.join("lib").join(name).exists()),
        );
    }
    json!({
        "schema": "gema.gvphase1.source_status.v1",
        "available": root.join("Makefile").exists()
            && build.join("bin/gvphase1-backend").exists()
            && build.join("lib/libcuda.so.1").exists(),
        "root": root.display().to_string(),
        "backend": build.join("bin/gvphase1-backend").display().to_string(),
        "libs": lib_status,
        "real_gpu_probe": root.join("tests/real_gpu_probe.c").exists(),
        "audit_script": root.join("scripts/audit_stubs.sh").exists(),
        "updated_at": now()
    })
}

fn cuda_proxy_default_state(ctx: &Ctx, session_id: &str) -> Value {
    let assignment = read_json(&gpu_assignment_path(ctx, session_id), json!({}));
    json!({
        "schema": "gema.cuda_proxy_state.v1",
        "session_id": session_id,
        "status": "not_attached",
        "ready": false,
        "data_plane": "gvphase1_binary_cuda_api_remoting",
        "role": "primary_terminal_visible_remote_gpu",
        "control_plane": "gema_fabric_bridge",
        "control_plane_role": "provisioning_tunnel_logs_checkpoints_lifecycle_only",
        "gpu_assignment": assignment,
        "source": gvphase1_host_source_status(),
        "frontend": {
            "env": {
                "LD_LIBRARY_PATH": "/opt/gema/gvphase1/lib:${LD_LIBRARY_PATH:-}",
                "GVPHASE1_ENDPOINT": "127.0.0.1:<tunnel-port>"
            },
            "libraries": ["libcuda.so.1", "libcudart.so", "libcublas.so", "libcudnn.so", "libcurand.so", "libcufft.so", "libnvrtc.so", "libnvidia-ml.so"]
        },
        "backend": {
            "binary": "gvphase1-backend",
            "placement": "attached_gpu_node",
            "provider": assignment["provider"].clone(),
            "endpoint": Value::Null
        },
        "validation": {
            "required_for_twin_claim": ["real_gpu_probe", "torch_cuda_probe"],
            "real_gpu_probe": "unknown",
            "torch_cuda_probe": "unknown"
        },
        "truth": {
            "twin_architecture_claim": "valid_only_when_cuda_proxy_ready_and_real_gpu_probe_passed",
            "fabric_bridge_class": "control_plane_not_cuda_data_plane",
            "accelerator_worker_class": "coarse_task_offload_not_terminal_visible_cuda"
        },
        "updated_at": now()
    })
}

fn cuda_proxy_status(ctx: &Ctx, session_id: &str) -> HResult<Value> {
    let state = read_json(&cuda_proxy_state_path(ctx, session_id), Value::Null);
    if state.is_object() {
        Ok(state)
    } else {
        Ok(cuda_proxy_default_state(ctx, session_id))
    }
}

fn cuda_proxy_contract(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let session_id = req["session_id"].as_str().ok_or("session_id required")?;
    let details = request_details(req);
    let mut state = cuda_proxy_default_state(ctx, session_id);
    let validation = details["validation"].clone();
    let endpoint = details["endpoint"].clone();
    let backend = details["backend"].clone();
    let ready = validation["real_gpu_probe_passed"]
        .as_bool()
        .unwrap_or(false)
        || validation["broad_real_gpu_probe_passed"]
            .as_bool()
            .unwrap_or(false);
    state["status"] = json!(if ready { "validated" } else { "configured" });
    state["ready"] = json!(ready);
    state["backend"]["endpoint"] = if endpoint.is_null() {
        Value::Null
    } else {
        endpoint
    };
    if backend.is_object() {
        state["backend"]["details"] = backend;
    }
    state["validation"] = json!({
        "real_gpu_probe": if ready { "passed" } else { validation["real_gpu_probe"].as_str().unwrap_or("unknown") },
        "torch_cuda_probe": validation["torch_cuda_probe"].as_str().unwrap_or("unknown"),
        "report": validation,
        "required_for_twin_claim": ["real_gpu_probe", "torch_cuda_probe"]
    });
    state["updated_at"] = json!(now());
    write_json_atomic(&cuda_proxy_state_path(ctx, session_id), &state)?;
    write_fabric_mirrors(ctx, session_id, "cuda_proxy_state.json", &state)?;
    append_runtime_event(
        ctx,
        session_id,
        "cuda_proxy_contract_written",
        json!({"ready": ready, "data_plane": state["data_plane"].clone(), "backend": state["backend"].clone()}),
        req["actor"].as_str().unwrap_or("system"),
        if ready { "info" } else { "warning" },
    )?;
    Ok(state)
}

fn default_state_ownership() -> Value {
    json!({
        "workspace_files": "workspace_volume",
        "runtime_manifest": "image_manifest",
        "checkpoint_policy": "checkpoint_policy.json",
        "snapshots": "snapshots.json",
        "command_intent": "hostd.json and runtime.events.jsonl",
        "gpu_assignment": "gpu_assignment.json",
        "process_state": "active_execution_worker_only",
        "agent_decisions": "runtime.events.jsonl"
    })
}

fn json_sha256(value: &Value) -> String {
    let encoded = serde_json::to_vec(value).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(encoded);
    format!("sha256:{:x}", hasher.finalize())
}

fn request_details(req: &Value) -> Value {
    if req["details"].is_object() {
        req["details"].clone()
    } else {
        json!({})
    }
}

fn value_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|item| !item.trim().is_empty())
        .map(|item| item.trim().to_string())
}

fn select_accelerator_profile(ctx: &Ctx, details: &Value, hostd: &Value) -> Value {
    let requested_lane = value_string(&details["cuda_lane"])
        .or_else(|| value_string(&hostd["launch_request"]["cuda_lane"]))
        .unwrap_or_else(|| "cu121".to_string());
    let requested_profile = value_string(&details["profile"])
        .or_else(|| value_string(&details["image_id"]))
        .or_else(|| value_string(&hostd["launch_request"]["profile"]));
    let profiles = rootfs_profiles(ctx);
    let selected = requested_profile
        .as_deref()
        .and_then(|wanted| {
            profiles.iter().find(|profile| {
                profile["id"].as_str() == Some(wanted)
                    || profile["profile"].as_str() == Some(wanted)
                    || profile["rootfs"]
                        .as_str()
                        .is_some_and(|rootfs| rootfs.ends_with(wanted))
            })
        })
        .or_else(|| {
            profiles.iter().find(|profile| {
                profile["capabilities"]["ml"]["lanes"]
                    .as_object()
                    .is_some_and(|lanes| lanes.contains_key(&requested_lane))
            })
        })
        .or_else(|| {
            profiles
                .iter()
                .find(|profile| profile["profile"].as_str() == Some("gpu-ml"))
        });
    selected.cloned().unwrap_or_else(|| {
        json!({
            "id": format!("noble-ml-{requested_lane}"),
            "profile": "gpu-ml",
            "rootfs": ctx.root.join("rootfs").join(format!("noble-ml-{requested_lane}")).display().to_string(),
            "capabilities": {"ml": {"lanes": {requested_lane.clone(): {"available": false}}}},
            "compatibility": {"gpu_vendors": ["cpu", "nvidia"], "runtime": ["gema-native"], "requires": {"cuda": requested_lane}}
        })
    })
}

fn build_execution_contract(ctx: &Ctx, session_id: &str, req: &Value) -> HResult<Value> {
    let session_dir = ctx.session_dir(session_id);
    fs::create_dir_all(&session_dir).map_err(|err| err.to_string())?;
    let details = request_details(req);
    let hostd = read_json(&session_dir.join("hostd.json"), json!({}));
    let image = select_accelerator_profile(ctx, &details, &hostd);
    let command = if details["command"].is_array() {
        details["command"].clone()
    } else {
        hostd["command"].clone()
    };
    let cwd = value_string(&details["cwd"]).unwrap_or_else(|| "/home/gema/workspace".to_string());
    let cuda_lane = value_string(&details["cuda_lane"])
        .or_else(|| value_string(&hostd["launch_request"]["cuda_lane"]))
        .or_else(|| {
            image["capabilities"]["ml"]["lanes"]
                .as_object()
                .and_then(|lanes| lanes.keys().next().cloned())
        })
        .unwrap_or_else(|| "cu121".to_string());
    let mut contract = json!({
        "schema": "gema.execution_contract.v1",
        "id": Uuid::new_v4().to_string(),
        "session_id": session_id,
        "created_at": now(),
        "updated_at": now(),
        "mode": "twin_compute_with_cuda_api_remoting",
        "runtime_identity": {
            "session_dir": session_dir.display().to_string(),
            "workspace": ctx.workspace_dir(session_id).display().to_string(),
            "controller": "persistent_cpu_runtime",
            "accelerator": "detachable_gpu_execution_twin"
        },
        "command": {
            "argv": command,
            "cwd": cwd,
            "source": if hostd["command"].is_array() { "hostd_launch" } else { "contract_request" },
            "resume_candidates": read_json(&recovery_plan_path(ctx, session_id), json!({}))["resume_contract_candidates"].clone()
        },
        "image": {
            "id": image["id"].clone(),
            "profile": image["profile"].clone(),
            "rootfs": image["rootfs"].clone(),
            "checksum": image["checksum"].clone(),
            "wheel_only": image["capabilities"]["ml"]["wheel_only"].clone(),
            "cuda_lane": cuda_lane,
            "capabilities": image["capabilities"].clone()
        },
        "gpu_requirements": {
            "vendor": details["gpu_vendor"].as_str().unwrap_or("nvidia"),
            "cuda_lane": cuda_lane,
            "min_vram_bytes": details["min_vram_bytes"].as_u64().unwrap_or(0),
            "require_gpu": details["require_gpu"].as_bool().unwrap_or(false)
        },
        "cuda_proxy": {
            "required_for_terminal_visible_gpu": details["require_terminal_visible_gpu"].as_bool().unwrap_or(true),
            "primary_data_plane": "gvphase1_binary_cuda_api_remoting",
            "frontend_env": {
                "LD_LIBRARY_PATH": "/opt/gema/gvphase1/lib:${LD_LIBRARY_PATH:-}",
                "GVPHASE1_ENDPOINT": "127.0.0.1:<tunnel-port>"
            },
            "backend_binary": "gvphase1-backend",
            "control_plane": "gema_fabric_bridge",
            "control_plane_role": "provisioning_tunnel_logs_checkpoints_lifecycle_only",
            "validation_gate": ["real_gpu_probe", "torch_cuda_probe"]
        },
        "checkpoint_policy": read_checkpoint_policy(ctx, session_id),
        "recovery_plan": read_json(&recovery_plan_path(ctx, session_id), json!({})),
        "state_ownership": default_state_ownership(),
        "governance": {
            "confidence_gate": details["confidence_threshold"].as_f64().unwrap_or(0.97),
            "cost_policy": details.get("cost_policy").cloned().unwrap_or(Value::Null),
            "automatic_writes": "allowed_only_when_contract_and_policy_are_present",
            "transparent_gpu_process_teleportation": false
        },
        "worker_protocol": {
            "attach_strategy": "start_gvphase1_backend_on_gpu_node_and_tunnel_cuda_rpc",
            "detach_strategy": "drain_checkpoint_unmount",
            "fabric_bridge_role": "control_plane",
            "accelerator_worker_role": "coarse_task_offload_fallback",
            "cuda_proxy_role": "primary_cuda_data_plane",
            "resume_truth": "framework_checkpoint_or_application_resume"
        }
    });
    contract["checksum"] = json!(json_sha256(&contract));
    Ok(contract)
}

fn write_execution_contract(ctx: &Ctx, session_id: &str, req: &Value) -> HResult<Value> {
    let contract = build_execution_contract(ctx, session_id, req)?;
    write_json_atomic(&execution_contract_path(ctx, session_id), &contract)?;
    write_fabric_mirrors(ctx, session_id, "execution_contract.json", &contract)?;
    append_runtime_event(
        ctx,
        session_id,
        "execution_contract_written",
        json!({"contract": execution_contract_path(ctx, session_id).display().to_string(), "checksum": contract["checksum"].clone(), "mode": contract["mode"].clone()}),
        req["actor"].as_str().unwrap_or("system"),
        "info",
    )?;
    Ok(contract)
}

fn read_or_create_execution_contract(ctx: &Ctx, session_id: &str, req: &Value) -> HResult<Value> {
    let existing = read_json(&execution_contract_path(ctx, session_id), Value::Null);
    if existing.is_object() {
        return Ok(existing);
    }
    write_execution_contract(ctx, session_id, req)
}

fn fabric_node_from_value(value: &Value) -> Value {
    json!({
        "id": value_string(&value["id"]).or_else(|| value_string(&value["node_id"])).unwrap_or_else(|| Uuid::new_v4().to_string()),
        "role": value["role"].as_str().unwrap_or("worker"),
        "provider": value["provider"].as_str().unwrap_or("unknown"),
        "gpu": value["gpu"].clone(),
        "cuda_lane": value["cuda_lane"].as_str().unwrap_or("auto"),
        "transport": value["transport"].clone(),
        "status": value["status"].as_str().unwrap_or("pending"),
        "last_seen_at": value["last_seen_at"].as_str().unwrap_or("").to_string()
    })
}

fn build_fabric_manifest(ctx: &Ctx, session_id: &str, req: &Value) -> HResult<Value> {
    let details = request_details(req);
    let execution_contract = read_or_create_execution_contract(ctx, session_id, req)?;
    let nodes = details["nodes"]
        .as_array()
        .map(|items| items.iter().map(fabric_node_from_value).collect::<Vec<_>>())
        .unwrap_or_default();
    let raw_mesh = details["raw_tcp_mesh"].clone();
    let bridge = details["bridge"].clone();
    let provider_classification = details["provider_classification"].clone();
    let mut manifest = json!({
        "schema": "gema.fabric_manifest.v1",
        "session_id": session_id,
        "created_at": now(),
        "updated_at": now(),
        "fabric": {
            "name": "Gema Fabric",
            "mode": details["mode"].as_str().unwrap_or("control_overlay"),
            "transport": details["transport"].as_str().unwrap_or("gema-ssh-stdio-bridge"),
            "raw_mesh_required": details["raw_mesh_required"].as_bool().unwrap_or(false),
            "classification": "control_plane",
            "truth_contract": "fabric bridge is control, lifecycle, transfer, tunnel, checkpoint, and recovery infrastructure; it is not the CUDA data plane"
        },
        "execution_contract": {
            "id": execution_contract["id"].clone(),
            "checksum": execution_contract["checksum"].clone(),
            "mode": execution_contract["mode"].clone(),
            "image": execution_contract["image"].clone(),
            "checkpoint_policy": execution_contract["checkpoint_policy"].clone()
        },
        "nodes": nodes,
        "provider_classification": if provider_classification.is_null() { json!({}) } else { provider_classification },
        "data_plane": {
            "primary": {
                "name": "GVPhase1 CUDA API remoting",
                "kind": "gvphase1_binary_cuda_api_remoting",
                "status": cuda_proxy_status(ctx, session_id)?["status"].clone(),
                "ready": cuda_proxy_status(ctx, session_id)?["ready"].clone(),
                "state": cuda_proxy_status(ctx, session_id)?
            },
            "control_bridge": if bridge.is_null() { json!({"required": true, "transport": "gema-ssh-stdio-bridge", "status": "pending", "classification": "control_plane"}) } else { bridge },
            "coarse_offload": {"name": "accelerator_worker", "classification": "fallback_task_offload"},
            "raw_tcp_mesh": if raw_mesh.is_null() { json!({"required": false, "status": "unknown"}) } else { raw_mesh },
            "control_path": "ssh",
            "payload_limits": {"max_blob_bytes": 33554432}
        },
        "failure_policy": {
            "node_loss": "pause_or_resume_from_latest_verified_checkpoint",
            "controller_loss": "rebuild_fabric_from_manifest_and_worker_heartbeats",
            "split_brain": "reject_duplicate_active_worker_identity",
            "checkpoint_conflict": "newest_verified_lineage_wins_only_after_manifest_validation"
        },
        "recovery": {
            "resume": read_json(&ctx.workspace_dir(session_id).join(".gema/recovery/resume.json"), json!({})),
            "checkpoint_policy": read_checkpoint_policy(ctx, session_id),
            "recovery_plan": read_json(&recovery_plan_path(ctx, session_id), json!({}))
        }
    });
    manifest["checksum"] = json!(json_sha256(&manifest));
    Ok(manifest)
}

fn write_fabric_manifest(ctx: &Ctx, session_id: &str, req: &Value) -> HResult<Value> {
    let manifest = build_fabric_manifest(ctx, session_id, req)?;
    write_json_atomic(&fabric_manifest_path(ctx, session_id), &manifest)?;
    write_fabric_mirrors(ctx, session_id, "fabric_manifest.json", &manifest)?;
    append_runtime_event(
        ctx,
        session_id,
        "fabric_manifest_written",
        json!({"checksum": manifest["checksum"].clone(), "transport": manifest["fabric"]["transport"].clone(), "nodes": manifest["nodes"].as_array().map(|v| v.len()).unwrap_or(0)}),
        req["actor"].as_str().unwrap_or("system"),
        "info",
    )?;
    Ok(manifest)
}

fn fabric_status(ctx: &Ctx, session_id: &str) -> HResult<Value> {
    let manifest = read_json(&fabric_manifest_path(ctx, session_id), Value::Null);
    Ok(json!({
        "schema": "gema.fabric_status.v1",
        "session_id": session_id,
        "status": if manifest.is_object() { "configured" } else { "missing" },
        "manifest": manifest,
        "execution_contract": read_json(&execution_contract_path(ctx, session_id), Value::Null),
        "cuda_proxy": cuda_proxy_status(ctx, session_id)?,
        "accelerator": accelerator_status(ctx, session_id)?,
        "updated_at": now()
    }))
}

fn accelerator_phase(name: &str, status: &str, details: Value) -> Value {
    json!({"name": name, "status": status, "details": details, "at": now()})
}

fn accelerator_status(ctx: &Ctx, session_id: &str) -> HResult<Value> {
    let state = read_json(&accelerator_state_path(ctx, session_id), Value::Null);
    if state.is_object() {
        return Ok(state);
    }
    Ok(json!({
        "schema": "gema.accelerator_state.v1",
        "session_id": session_id,
        "status": "detached",
        "attached": false,
        "strategy": "stateful_execution_offload",
        "classification": "fallback_task_offload",
        "cuda_proxy": cuda_proxy_status(ctx, session_id)?,
        "contract": read_json(&execution_contract_path(ctx, session_id), Value::Null),
        "gpu_assignment": read_json(&gpu_assignment_path(ctx, session_id), json!({})),
        "updated_at": now()
    }))
}

fn accelerator_attach(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let session_id = req["session_id"].as_str().ok_or("session_id required")?;
    let details = request_details(req);
    let contract = read_or_create_execution_contract(ctx, session_id, req)?;
    let assignment = read_json(&gpu_assignment_path(ctx, session_id), json!({}));
    let cuda_proxy = cuda_proxy_status(ctx, session_id)?;
    let has_gpu = assignment.as_object().is_some_and(|item| !item.is_empty());
    let require_gpu = details["require_gpu"].as_bool().unwrap_or_else(|| {
        contract["gpu_requirements"]["require_gpu"]
            .as_bool()
            .unwrap_or(false)
    });
    let mut phases = vec![
        accelerator_phase(
            "contract_loaded",
            "passed",
            json!({"checksum": contract["checksum"].clone()}),
        ),
        accelerator_phase(
            "compatibility_checked",
            "passed",
            json!({"image": contract["image"].clone(), "gpu_requirements": contract["gpu_requirements"].clone()}),
        ),
        accelerator_phase(
            "workspace_ready",
            "passed",
            json!({"workspace": ctx.workspace_dir(session_id).display().to_string()}),
        ),
        accelerator_phase(
            "checkpoint_policy_checked",
            "passed",
            contract["checkpoint_policy"].clone(),
        ),
    ];
    let status = if has_gpu {
        phases.push(accelerator_phase(
            "gpu_assignment_ready",
            "passed",
            assignment.clone(),
        ));
        phases.push(accelerator_phase(
            "cuda_proxy_data_plane",
            if cuda_proxy["ready"].as_bool().unwrap_or(false) { "passed" } else { "required" },
            json!({"kind": "gvphase1_binary_cuda_api_remoting", "ready": cuda_proxy["ready"].clone(), "state": cuda_proxy["status"].clone()}),
        ));
        phases.push(accelerator_phase(
            "fabric_control_bridge",
            "passed",
            json!({"classification": "control_plane", "role": "provisioning_tunnel_logs_checkpoints_lifecycle"}),
        ));
        "attached"
    } else if require_gpu {
        phases.push(accelerator_phase(
            "gpu_assignment_ready",
            "blocked",
            json!({"reason": "no_gpu_assignment"}),
        ));
        phases.push(accelerator_phase(
            "execution_twin_ready",
            "blocked",
            json!({"reason": "accelerator_not_allocated"}),
        ));
        "blocked_no_gpu_assignment"
    } else {
        phases.push(accelerator_phase(
            "gpu_assignment_ready",
            "waiting",
            json!({"reason": "accelerator_not_allocated"}),
        ));
        phases.push(accelerator_phase(
            "execution_twin_ready",
            "waiting",
            json!({"reason": "waiting_for_gpu_worker"}),
        ));
        "waiting_for_accelerator"
    };
    let state = json!({
        "schema": "gema.accelerator_state.v1",
        "session_id": session_id,
        "status": status,
        "attached": status == "attached",
        "strategy": "twin_compute_cuda_api_remoting",
        "classification": if cuda_proxy["ready"].as_bool().unwrap_or(false) { "true_cuda_proxy_twin" } else { "gpu_assignment_without_validated_cuda_proxy" },
        "controller": "persistent_cpu_runtime",
        "accelerator": "detachable_gpu_execution_twin",
        "cuda_proxy": cuda_proxy,
        "control_bridge": {"name": "gema-fabric-bridge", "classification": "control_plane"},
        "coarse_offload": {"name": "gema-accelerator-worker", "classification": "fallback_task_offload"},
        "contract_id": contract["id"].clone(),
        "contract_checksum": contract["checksum"].clone(),
        "image": contract["image"].clone(),
        "gpu_assignment": assignment,
        "state_ownership": contract["state_ownership"].clone(),
        "phases": phases,
        "next_action": match status {
            "attached" => "route_accelerated_workload_to_execution_twin",
            "blocked_no_gpu_assignment" => "allocate_compatible_gpu_or_escalate",
            _ => "wait_for_compatible_gpu_worker"
        },
        "updated_at": now()
    });
    write_json_atomic(&accelerator_state_path(ctx, session_id), &state)?;
    write_fabric_mirrors(ctx, session_id, "accelerator_state.json", &state)?;
    append_runtime_event(
        ctx,
        session_id,
        "accelerator_attach_requested",
        json!({"status": status, "attached": status == "attached", "contract_checksum": contract["checksum"].clone()}),
        req["actor"].as_str().unwrap_or("system"),
        if status == "blocked_no_gpu_assignment" {
            "warning"
        } else {
            "info"
        },
    )?;
    Ok(json!({"ok": status != "blocked_no_gpu_assignment", "accelerator": state}))
}

fn accelerator_detach(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let session_id = req["session_id"].as_str().ok_or("session_id required")?;
    let mode = req["mode"].as_str().unwrap_or("drain");
    let previous = accelerator_status(ctx, session_id)?;
    let contract = read_json(&execution_contract_path(ctx, session_id), Value::Null);
    let state = json!({
        "schema": "gema.accelerator_state.v1",
        "session_id": session_id,
        "status": "detached",
        "attached": false,
        "strategy": "twin_compute_cuda_api_remoting",
        "classification": "detached_cuda_proxy_twin",
        "cuda_proxy": cuda_proxy_status(ctx, session_id)?,
        "detach_mode": mode,
        "previous_status": previous["status"].clone(),
        "contract_id": contract["id"].clone(),
        "contract_checksum": contract["checksum"].clone(),
        "state_ownership": if contract.is_object() { contract["state_ownership"].clone() } else { default_state_ownership() },
        "phases": [
            accelerator_phase("detach_requested", "passed", json!({"mode": mode})),
            accelerator_phase("checkpoint_boundary", "required", json!({"policy": "framework_or_application_checkpoint_before_gpu_release"})),
            accelerator_phase("execution_twin_detached", "passed", json!({}))
        ],
        "updated_at": now()
    });
    write_json_atomic(&accelerator_state_path(ctx, session_id), &state)?;
    write_fabric_mirrors(ctx, session_id, "accelerator_state.json", &state)?;
    append_runtime_event(
        ctx,
        session_id,
        "accelerator_detached",
        json!({"mode": mode, "previous_status": previous["status"].clone()}),
        req["actor"].as_str().unwrap_or("system"),
        "info",
    )?;
    Ok(json!({"ok": true, "accelerator": state}))
}

fn configure_checkpoint_policy(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let session_id = req["session_id"].as_str().ok_or("session_id required")?;
    let backend = req["backend"].as_str().unwrap_or("managed");
    if backend != "managed" {
        return Err("managed R2 is the only supported checkpoint backend".to_string());
    }
    let interval = req["interval_seconds"]
        .as_u64()
        .unwrap_or(300)
        .clamp(60, 86_400);
    let mut policy = read_checkpoint_policy(ctx, session_id);
    policy["enabled"] = json!(req["enabled"].as_bool().unwrap_or(true));
    policy["backend"] = json!(backend);
    policy["checkpoint_target"] = json!(backend);
    policy["workspace_policy"] = json!("managed");
    policy["interval_seconds"] = json!(interval);
    if policy["last_snapshot_epoch"].as_u64().unwrap_or(0) == 0 {
        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|v| v.as_secs())
            .unwrap_or(0);
        policy["last_snapshot_epoch"] = json!(now_epoch);
        policy["next_snapshot_not_before_epoch"] = json!(now_epoch.saturating_add(interval));
        policy["next_snapshot_not_before_at"] =
            json!((Local::now() + chrono::Duration::seconds(interval as i64)).to_rfc3339());
    }
    if let Some(restore_policy) = req["restore_policy"].as_str() {
        if !["latest-consistent", "workspace-only", "process-aware"].contains(&restore_policy) {
            return Err("invalid restore policy".to_string());
        }
        policy["restore_policy"] = json!(restore_policy);
    }
    if let Some(retention_policy) = req["retention_policy"].as_str() {
        if !["rolling", "pinned", "archival", "ephemeral"].contains(&retention_policy) {
            return Err("invalid retention policy".to_string());
        }
        policy["retention_policy"] = json!(retention_policy);
    }
    let policy = write_checkpoint_policy(ctx, session_id, policy)?;
    append_runtime_event(
        ctx,
        session_id,
        "checkpoint_policy_updated",
        policy.clone(),
        "system",
        "info",
    )?;
    Ok(policy)
}

fn rclone_lsf(config_path: Option<&Path>, remote: &str, max_depth: &str) -> Value {
    let mut cmd = Command::new("rclone");
    if let Some(config_path) = config_path {
        cmd.arg("--config").arg(config_path);
    }
    cmd.args(["lsf", remote, "--max-depth", max_depth]);
    match cmd.output() {
        Ok(out) => json!({
            "ok": out.status.success(),
            "status": out.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&out.stdout).trim().to_string(),
            "stderr": String::from_utf8_lossy(&out.stderr).trim().to_string()
        }),
        Err(err) => json!({"ok": false, "status": 127, "stdout": "", "stderr": err.to_string()}),
    }
}

fn checkpoint_remote_for_backend(
    ctx: &Ctx,
    session_id: &str,
    backend: &str,
) -> HResult<(Option<PathBuf>, String, Value)> {
    if backend != "managed" {
        return Err("managed R2 is the only supported checkpoint backend".to_string());
    }
    let state = read_r2_state(ctx, session_id);
    if !state["enabled"].as_bool().unwrap_or(false) {
        return Err("managed storage not enabled".to_string());
    }
    let remote = state["snapshot_remote"].as_str().unwrap_or("").to_string();
    if remote.is_empty() {
        return Err("managed snapshot remote missing".to_string());
    }
    Ok((None, remote, state))
}

fn verify_snapshot_artifacts(
    ctx: &Ctx,
    session_id: &str,
    snapshot: &Value,
    backend: &str,
) -> Value {
    let id = snapshot["id"].as_str().unwrap_or("");
    if id.is_empty() {
        return json!({"ok": false, "error": "snapshot id missing"});
    }
    let archive = snapshot["details"]["path"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| snapshot_archive_path(ctx, session_id, id));
    let expected_checksum = snapshot["details"]["checksum"].as_str().unwrap_or("");
    let local_checksum = if archive.exists() {
        sha256_file(&archive)
            .map(Value::String)
            .unwrap_or_else(|err| json!({"error": err}))
    } else {
        Value::Null
    };
    let local_ok = archive.exists()
        && !expected_checksum.is_empty()
        && local_checksum.as_str() == Some(expected_checksum);

    let remote = checkpoint_remote_for_backend(ctx, session_id, backend);
    let remote_check = match remote {
        Ok((config_path, remote, state)) => {
            let listing = rclone_lsf(config_path.as_deref(), &remote, "1");
            let stdout = listing["stdout"].as_str().unwrap_or("");
            let archive_name = format!("{id}.tar");
            let manifest_ok = stdout.lines().any(|line| line.trim() == "snapshots.json");
            let archive_ok = stdout.lines().any(|line| line.trim() == archive_name);
            json!({
                "ok": listing["ok"].as_bool().unwrap_or(false) && archive_ok && manifest_ok,
                "backend": backend,
                "remote": remote,
                "archive_present": archive_ok,
                "manifest_present": manifest_ok,
                "listing": listing,
                "state": state
            })
        }
        Err(err) => json!({"ok": false, "backend": backend, "error": err}),
    };

    json!({
        "ok": local_ok && remote_check["ok"].as_bool().unwrap_or(false),
        "snapshot_id": id,
        "verified_at": now(),
        "local": {
            "ok": local_ok,
            "archive": archive.display().to_string(),
            "archive_present": archive.exists(),
            "expected_checksum": expected_checksum,
            "actual_checksum": local_checksum
        },
        "remote": remote_check
    })
}

fn build_persistence_status(ctx: &Ctx, session_id: &str, verify: bool) -> HResult<Value> {
    let mut policy = read_checkpoint_policy(ctx, session_id);
    let backend = policy["backend"].as_str().unwrap_or("managed").to_string();
    let latest_snapshot = latest_completed_snapshot(ctx, session_id);
    let latest_process_checkpoint = latest_completed_process_checkpoint(ctx, session_id);
    let remote_state = checkpoint_remote_for_backend(ctx, session_id, &backend)
        .map(|(_, remote, state)| json!({"ok": true, "backend": backend, "remote": remote, "state": state}))
        .unwrap_or_else(|err| json!({"ok": false, "backend": backend, "error": err}));
    let verification = if verify {
        latest_snapshot
            .as_ref()
            .map(|snapshot| verify_snapshot_artifacts(ctx, session_id, snapshot, &backend))
            .unwrap_or_else(|| json!({"ok": false, "error": "no completed snapshot found"}))
    } else {
        policy["last_verified_restore_point"].clone()
    };
    if verify {
        policy["last_verified_restore_point"] = verification.clone();
        let _ = write_checkpoint_policy(ctx, session_id, policy.clone());
    }
    let interval = policy["interval_seconds"].as_u64().unwrap_or(300).max(60);
    let last_epoch = policy["last_snapshot_epoch"].as_u64().unwrap_or(0);
    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs())
        .unwrap_or(0);
    let next_in_seconds = if policy["enabled"].as_bool().unwrap_or(true) {
        Some(interval.saturating_sub(now_epoch.saturating_sub(last_epoch)))
    } else {
        None
    };
    let status = json!({
        "schema": "gema.persistence_status.v1",
        "session_id": session_id,
        "policy": policy,
        "workspace": {
            "path": session_workspace_path(ctx, session_id).display().to_string(),
            "storage": read_json(&storage_latest_path(ctx, session_id), json!({}))
        },
        "latest_snapshot": latest_snapshot,
        "latest_process_checkpoint": latest_process_checkpoint,
        "remote": remote_state,
        "verification": verification,
        "next_checkpoint_in_seconds": next_in_seconds,
        "recovery_semantics": {
            "what_persists": ["workspace files", "session metadata", "runtime filesystem diff when present", "process checkpoint when enabled and compatible"],
            "restore_order": ["latest verified checkpoint", "latest completed checkpoint", "workspace state", "process checkpoint when compatible"],
            "not_guaranteed": ["open network connections", "remote peer state", "unsynced application buffers", "GPU VRAM outside application checkpoints"]
        },
        "updated_at": now()
    });
    write_json_atomic(&persistence_status_path(ctx, session_id), &status)?;
    Ok(status)
}

fn persistence_policy_action(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let policy = configure_checkpoint_policy(ctx, req)?;
    let status = build_persistence_status(
        ctx,
        req["session_id"].as_str().ok_or("session_id required")?,
        false,
    )?;
    Ok(json!({"ok": true, "persistence_policy": policy, "persistence_status": status}))
}

fn checkpoint_loop(ctx: Ctx) {
    loop {
        thread::sleep(Duration::from_secs(30));
        let root = ctx.root.join("workspaces");
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let session_id = entry.file_name().to_string_lossy().to_string();
            if session_id.is_empty() {
                continue;
            }
            let lifecycle = read_json(&lifecycle_path(&ctx, &session_id), json!({}));
            let state = lifecycle["state"].as_str().unwrap_or("");
            if !matches!(state, "ready" | "running" | "recovering") {
                continue;
            }
            let mut policy = read_checkpoint_policy(&ctx, &session_id);
            if !policy["enabled"].as_bool().unwrap_or(true) {
                continue;
            }
            let interval = policy["interval_seconds"].as_u64().unwrap_or(300).max(60);
            let last = policy["last_snapshot_epoch"].as_u64().unwrap_or(0);
            let now_epoch = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|v| v.as_secs())
                .unwrap_or(0);
            if now_epoch.saturating_sub(last) < interval {
                continue;
            }
            let marker = ctx.session_dir(&session_id).join("checkpoint.lock");
            if OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)
                .is_err()
            {
                continue;
            }
            let snapshot_id = format!(
                "auto-{}-{}",
                Local::now().format("%Y%m%d%H%M%S"),
                &Uuid::new_v4().to_string()[..8]
            );
            let result = create_local_snapshot(
                &ctx,
                &session_id,
                Some(&snapshot_id),
                "checkpoint-scheduler",
            );
            let backend = "managed".to_string();
            let sync = match &result {
                Ok(value) if value["ok"].as_bool().unwrap_or(false) => {
                    r2_sync(&ctx, &session_id, "snapshots", "push")
                }
                _ => Err("snapshot failed before sync".to_string()),
            };
            let verification = if sync
                .as_ref()
                .map(|v| v["ok"].as_bool().unwrap_or(false))
                .unwrap_or(false)
            {
                latest_completed_snapshot(&ctx, &session_id)
                    .map(|snapshot| {
                        verify_snapshot_artifacts(&ctx, &session_id, &snapshot, &backend)
                    })
                    .unwrap_or_else(|| json!({"ok": false, "error": "no completed snapshot found"}))
            } else {
                json!({"ok": false, "error": "sync failed before verification"})
            };
            policy["last_snapshot_epoch"] = json!(now_epoch);
            policy["last_snapshot_at"] = json!(now());
            policy["last_snapshot_id"] = json!(snapshot_id);
            policy["last_result"] = match result {
                Ok(value) => value,
                Err(err) => json!({"ok": false, "error": err}),
            };
            policy["last_sync"] = match sync {
                Ok(value) => value,
                Err(err) => json!({"ok": false, "error": err}),
            };
            policy["last_verified_restore_point"] = verification;
            let _ = write_checkpoint_policy(&ctx, &session_id, policy);
            let _ = build_persistence_status(&ctx, &session_id, false);
            let _ = fs::remove_file(marker);
        }
    }
}

fn collect_metrics(ctx: &Ctx, session_id: &str) -> HResult<Value> {
    let base = ctx.session_dir(session_id);
    let cgroup = PathBuf::from(format!("/sys/fs/cgroup/gema-system/{session_id}"));
    let meta = read_json(&base.join("meta.json"), json!({}));
    let network = read_json(&base.join("network.json"), json!({}));
    let lifecycle = read_json(&lifecycle_path(ctx, session_id), json!({}));
    let previous = read_json(
        &ctx.session_dir(session_id).join("metrics.latest.json"),
        json!({}),
    );
    let cpu_stat = read_key_values(&cgroup.join("cpu.stat"));
    let cpu_usage = cpu_stat
        .get("usage_usec")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let sampled_at_epoch = unix_now();
    let cpu_percent = previous["cpu"]["usage_usec"]
        .as_i64()
        .zip(previous["sampled_at_epoch"].as_f64())
        .and_then(|(prev_cpu, prev_time)| {
            let elapsed = sampled_at_epoch - prev_time;
            if elapsed > 0.0 {
                Some(((cpu_usage - prev_cpu).max(0) as f64 / 1_000_000.0) / elapsed * 100.0)
            } else {
                None
            }
        });
    let workspace = meta["workspace"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.workspace_dir(session_id));
    let host_veth = network["host_veth"].as_str().unwrap_or("");
    let net_base = PathBuf::from(format!("/sys/class/net/{host_veth}/statistics"));
    let assignment = read_json(&gpu_assignment_path(ctx, session_id), json!({}));
    let ports = read_json(
        &session_port_allocations_path(ctx, session_id),
        json!({"active": []}),
    );
    let router_records = read_ports_jsonl(ctx, session_id);
    let empty_ports = Vec::new();
    let services: Vec<Value> = ports["active"]
        .as_array()
        .unwrap_or(&empty_ports)
        .iter()
        .map(|item| {
            let protocol = item["protocol"].as_str().unwrap_or("tcp");
            let host_port = item["host_port"].as_u64().map(|v| v as u16);
            let router_alive = router_records.iter().any(|record| {
                record["protocol"].as_str().unwrap_or("tcp") == protocol
                    && record["host_port"].as_u64() == item["host_port"].as_u64()
                    && record["pid"].as_i64().is_some_and(pid_alive)
            });
            json!({
                "protocol": item["protocol"].clone(),
                "visibility": item["visibility"].clone(),
                "host_port": item["host_port"].clone(),
                "guest_port": item["guest_port"].clone(),
                "ready": if protocol == "udp" { router_alive } else { host_port.is_some_and(port_ready) },
                "router_alive": router_alive,
            })
        })
        .collect();
    let sample = json!({
        "session_id": session_id,
        "sampled_at": now(),
        "sampled_at_epoch": sampled_at_epoch,
        "state": lifecycle["state"].clone(),
        "cpu": {"usage_usec": cpu_usage, "user_usec": cpu_stat.get("user_usec").cloned().unwrap_or(json!(0)), "system_usec": cpu_stat.get("system_usec").cloned().unwrap_or(json!(0)), "percent": cpu_percent},
        "ram": {"current_bytes": read_i64(&cgroup.join("memory.current"), 0), "peak_bytes": read_i64(&cgroup.join("memory.peak"), 0), "max_bytes": read_text(&cgroup.join("memory.max"))},
        "disk": {"workspace_bytes": directory_size_bytes(&workspace)},
        "network": {"interface": host_veth, "rx_bytes": read_i64(&net_base.join("rx_bytes"), 0), "tx_bytes": read_i64(&net_base.join("tx_bytes"), 0), "rx_packets": read_i64(&net_base.join("rx_packets"), 0), "tx_packets": read_i64(&net_base.join("tx_packets"), 0)},
        "services": services,
        "security": read_json(&base.join("security.json"), json!({})),
        "processes": {"count": read_i64(&cgroup.join("pids.current"), cgroup_pids(session_id).len() as i64), "max": read_text(&cgroup.join("pids.max")), "pids": cgroup_pids(session_id)},
        "uptime_seconds": 0,
        "gpu": { "assignment": assignment, "live": gpu_metrics(&assignment) }
    });
    write_json_atomic(
        &ctx.session_dir(session_id).join("metrics.latest.json"),
        &sample,
    )?;
    append_jsonl(
        &ctx.session_dir(session_id).join("metrics.samples.jsonl"),
        &sample,
    )?;
    Ok(sample)
}

fn active_session_ids(ctx: &Ctx) -> Vec<String> {
    let Ok(entries) = fs::read_dir(ctx.root.join("sessions")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|id| {
            matches!(
                read_json(&lifecycle_path(ctx, id), json!({}))["state"].as_str(),
                Some("running" | "ready" | "recovering" | "starting" | "stopping")
            )
        })
        .collect()
}

fn metrics_loop(ctx: Ctx) {
    loop {
        for id in active_session_ids(&ctx) {
            if let Err(err) = collect_metrics(&ctx, &id)
                .and_then(|_| collect_storage(&ctx, &id, true).map(|_| ()))
            {
                log(&ctx, &format!("metrics_error session={id} error={err}"));
            }
        }
        thread::sleep(ctx.metrics_interval);
    }
}

fn reconcile_sessions(ctx: &Ctx) -> HResult<()> {
    for id in active_session_ids(ctx) {
        let hostd_doc = read_json(&ctx.session_dir(&id).join("hostd.json"), json!({}));
        let pid = hostd_doc["pid"].as_i64();
        let live_workload = session_has_live_workload(ctx, &id);
        if live_workload || pid.is_some_and(pid_alive) {
            let _ = scan_session_processes(ctx, &id);
            append_runtime_event(
                ctx,
                &id,
                "recovery_attempted",
                json!({"reason": "daemon_reconcile", "pid": pid}),
                "system",
                "info",
            )?;
            transition(
                ctx,
                &id,
                "recovering",
                "daemon_reconcile",
                json!({"pid": pid}),
                false,
            )?;
            transition(
                ctx,
                &id,
                "running",
                "process_alive_after_daemon_restart",
                json!({"pid": pid, "live_workload": live_workload}),
                false,
            )?;
        } else {
            append_runtime_event(
                ctx,
                &id,
                "recovery_attempted",
                json!({"reason": "daemon_reconcile_missing_process", "pid": pid}),
                "system",
                "warning",
            )?;
            transition(
                ctx,
                &id,
                "failed",
                "process_missing_after_daemon_restart",
                json!({"pid": pid}),
                false,
            )?;
        }
    }
    Ok(())
}

fn port_ready(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).is_ok()
}

fn tcp_addr_ready(addr: &str, timeout: Duration) -> bool {
    let Ok(socket_addr) = addr.parse() else {
        return false;
    };
    TcpStream::connect_timeout(&socket_addr, timeout).is_ok()
}

fn http_addr_ready(addr: &str, path: &str, timeout: Duration) -> bool {
    let Ok(socket_addr) = addr.parse() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&socket_addr, timeout) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        if path.starts_with('/') { path } else { "/" },
        addr
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = [0u8; 64];
    let Ok(n) = stream.read(&mut response) else {
        return false;
    };
    let head = String::from_utf8_lossy(&response[..n]);
    head.starts_with("HTTP/1.0 2")
        || head.starts_with("HTTP/1.1 2")
        || head.starts_with("HTTP/1.0 3")
        || head.starts_with("HTTP/1.1 3")
}

fn session_guest_ip(ctx: &Ctx, session_id: &str) -> Option<String> {
    read_json(&ctx.session_dir(session_id).join("network.json"), json!({}))["guest_ip"]
        .as_str()
        .map(str::to_string)
}

fn service_ready(ctx: &Ctx, session_id: &str, guest_port: u16, req: &Value) -> bool {
    let Some(guest_ip) = session_guest_ip(ctx, session_id) else {
        return false;
    };
    let addr = format!("{guest_ip}:{guest_port}");
    let timeout = Duration::from_millis(req["health_timeout_ms"].as_u64().unwrap_or(750));
    if let Some(path) = req["health_path"].as_str().filter(|v| !v.trim().is_empty()) {
        http_addr_ready(&addr, path, timeout)
    } else {
        tcp_addr_ready(&addr, timeout)
    }
}

fn wait_for_service_ready(
    ctx: &Ctx,
    session_id: &str,
    guest_port: u16,
    req: &Value,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if service_ready(ctx, session_id, guest_port, req) {
            return true;
        }
        thread::sleep(Duration::from_millis(200));
    }
    false
}

fn restart_allowed(policy: &str, exit_code: Option<i32>) -> bool {
    match policy {
        "always" => true,
        "on-failure" => exit_code != Some(0),
        _ => false,
    }
}

fn maybe_restart_session(ctx: &Ctx, session_id: &str, exit_code: Option<i32>) -> bool {
    if ctx.session_dir(session_id).join(".stop_requested").exists() {
        return false;
    }
    let metadata_path = ctx.session_dir(session_id).join("hostd.json");
    let mut metadata = read_json(&metadata_path, json!({}));
    let policy = metadata["restart_policy"]
        .as_str()
        .unwrap_or("never")
        .to_string();
    if !restart_allowed(&policy, exit_code) {
        return false;
    }
    let attempts = metadata["restart_attempts"].as_u64().unwrap_or(0);
    let max_attempts = metadata["restart_max_attempts"].as_u64().unwrap_or(3);
    if attempts >= max_attempts {
        let _ = append_runtime_event(
            ctx,
            session_id,
            "restart_limit_reached",
            json!({"restart_policy": policy, "attempts": attempts, "max_attempts": max_attempts, "exit_code": exit_code}),
            "system",
            "error",
        );
        return false;
    }
    let mut req = metadata["launch_request"].clone();
    if !req.is_object() {
        return false;
    }
    let next_attempt = attempts + 1;
    metadata["restart_attempts"] = json!(next_attempt);
    metadata["last_restart_at"] = json!(now());
    metadata["last_exit_code"] = exit_code.map_or(Value::Null, |v| json!(v));
    let _ = write_json_atomic(&metadata_path, &metadata);
    let _ = transition(
        ctx,
        session_id,
        "recovering",
        "restart_policy",
        json!({"restart_policy": policy, "attempt": next_attempt, "max_attempts": max_attempts, "exit_code": exit_code}),
        false,
    );
    let _ = append_runtime_event(
        ctx,
        session_id,
        "restart_attempted",
        json!({"restart_policy": policy, "attempt": next_attempt, "max_attempts": max_attempts, "exit_code": exit_code}),
        "system",
        "warning",
    );
    req["actor"] = json!("system");
    req["restart_attempt"] = json!(next_attempt);
    thread::sleep(Duration::from_secs(1));
    let _ = cleanup_runtime_preserve(ctx, session_id);
    match launch(ctx, &req) {
        Ok(result) => {
            let _ = append_runtime_event(
                ctx,
                session_id,
                "restart_completed",
                result,
                "system",
                "info",
            );
            true
        }
        Err(err) => {
            let _ = append_runtime_event(
                ctx,
                session_id,
                "restart_failed",
                json!({"error": err}),
                "system",
                "error",
            );
            false
        }
    }
}

fn monitor_process(ctx: Ctx, session_id: String, mut child: std::process::Child) {
    let pid = child.id() as i64;
    let exit_code = child.wait().ok().and_then(|s| s.code());
    let lifecycle = read_json(&lifecycle_path(&ctx, &session_id), json!({}));
    let state = lifecycle["state"].as_str().unwrap_or("");
    let expected = matches!(state, "stopping" | "stopped" | "failed")
        || ctx
            .session_dir(&session_id)
            .join(".stop_requested")
            .exists()
        || exit_code == Some(0);
    let _ = finish_process(
        &ctx,
        &session_id,
        "main",
        pid,
        exit_code,
        if expected {
            Some("session_stopped")
        } else {
            None
        },
        expected,
    );
    let _ = append_runtime_event(
        &ctx,
        &session_id,
        "process_exited",
        json!({"pid": pid, "exit_code": exit_code, "expected": expected, "state_at_exit": state}),
        "system",
        if expected { "info" } else { "error" },
    );
    if !expected {
        if maybe_restart_session(&ctx, &session_id, exit_code) {
            return;
        }
        let _ = transition(
            &ctx,
            &session_id,
            "failed",
            "process_exited",
            json!({"pid": pid, "exit_code": exit_code}),
            false,
        );
        let _ = release_session_ports(&ctx, &session_id, "system");
    } else if state != "stopping" && state != "stopped" {
        let _ = release_session_ports(&ctx, &session_id, "system");
    }
}

fn launch(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let session_id = req["session_id"]
        .as_str()
        .ok_or_else(|| "session_id required".to_string())?;
    let _ = fs::remove_file(ctx.session_dir(session_id).join(".stop_requested"));
    let policy = isolation_policy(req);
    if policy["lane"].as_str() == Some("secure")
        && !policy["gvisor_available"].as_bool().unwrap_or(false)
    {
        return Err("secure isolation lane requires runsc/gVisor on this host".to_string());
    }
    let launch_readiness = build_launch_readiness(ctx, req, true)?;
    if !launch_readiness["ok"].as_bool().unwrap_or(false) {
        return Err(format!(
            "launch readiness failed: {}",
            launch_readiness["blocking"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item["name"].as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_else(|| "unknown".to_string())
        ));
    }
    let host_preflight = require_host_probe(ctx, req)?;
    let selected_rootfs = rootfs_for_launch(ctx, req)?;
    let storage_quota = req["storage_quota_bytes"]
        .as_u64()
        .unwrap_or(ctx.default_storage_quota_bytes);
    set_storage_policy(
        ctx,
        session_id,
        Some(storage_quota),
        req["snapshot_retention_count"]
            .as_u64()
            .or(Some(ctx.snapshot_retention_count)),
        None,
    )?;
    let r2_direct_enabled = req["direct_r2_workspace"].as_bool().unwrap_or(true);
    let mut r2_state = read_r2_state(ctx, session_id);
    if r2_direct_enabled
        && !r2_state["enabled"].as_bool().unwrap_or(false)
        && req["user_id"]
            .as_str()
            .is_some_and(|user_id| !user_id.trim().is_empty() && user_id != "local")
    {
        let user_id = req["user_id"].as_str().unwrap();
        let prefix = format!("users/{user_id}");
        r2_state = configure_r2(ctx, session_id, None, Some(&prefix), true)?;
    }
    let direct_r2_active = r2_direct_enabled && r2_state["enabled"].as_bool().unwrap_or(false);
    let mut storage_volume = if direct_r2_active {
        let mountpoint = ctx.workspace_dir(session_id);
        if mounted_at(&mountpoint) && !rclone_mounted_at(&mountpoint) {
            let _ = Command::new("umount").arg(&mountpoint).status();
            if mounted_at(&mountpoint) && !rclone_mounted_at(&mountpoint) {
                let _ = Command::new("umount").arg("-l").arg(&mountpoint).status();
            }
        }
        let mut direct_policy = read_storage_policy(ctx, session_id)?;
        direct_policy["enforcement"]["mode"] = json!("direct-r2");
        direct_policy["enforcement"]["on_exceeded"] = json!("r2_backend_limit");
        direct_policy["enforcement"]["kernel_quota"] = json!(false);
        direct_policy["updated_at"] = json!(now());
        write_json_atomic(&storage_policy_path(ctx, session_id), &direct_policy)?;
        prepare_r2_mountpoint(&mountpoint)?;
        let volume = json!({
            "session_id": session_id,
            "mode": "direct-r2",
            "mountpoint": mountpoint.display().to_string(),
            "quota_bytes": storage_quota,
            "mounted": rclone_mount_healthy(&mountpoint),
            "direct_r2": true,
            "remote": r2_state["workspace_remote"].clone(),
            "updated_at": now()
        });
        write_storage_volume(ctx, session_id, &volume)?;
        append_runtime_event(
            ctx,
            session_id,
            "storage_volume_mounted",
            volume.clone(),
            "system",
            "info",
        )?;
        volume
    } else {
        return Err("managed R2 workspace storage is required; refusing local workspace volume".to_string());
    };
    let r2_workspace_mount =
        if direct_r2_active {
            let mountpoint = PathBuf::from(
                storage_volume["mountpoint"]
                    .as_str()
                    .unwrap_or(&ctx.workspace_dir(session_id).display().to_string()),
            );
            if mounted_at(&mountpoint) && !rclone_mounted_at(&mountpoint) {
                let _ = Command::new("umount").arg(&mountpoint).status();
                if mounted_at(&mountpoint) && !rclone_mounted_at(&mountpoint) {
                    let _ = Command::new("umount").arg("-l").arg(&mountpoint).status();
                }
            }
            match r2_mount(ctx, session_id, "workspace") {
                Ok(value) if value["ok"].as_bool().unwrap_or(false) => value,
                Ok(value) => {
                    let _ = unmount_workspace_volume(ctx, session_id, false);
                    return Err(format!("direct R2 workspace mount failed: {value}"));
                }
                Err(err) => {
                    let _ = unmount_workspace_volume(ctx, session_id, false);
                    return Err(format!("direct R2 workspace mount failed: {err}"));
                }
            }
        } else {
            Value::Null
        };
    if direct_r2_active && r2_workspace_mount["ok"].as_bool().unwrap_or(false) {
        storage_volume["mounted"] = json!(true);
        storage_volume["healthy"] = r2_workspace_mount["healthy"].clone();
        storage_volume["fstype"] = r2_workspace_mount["fstype"].clone();
        storage_volume["remote"] = r2_workspace_mount["remote"].clone();
        storage_volume["updated_at"] = json!(now());
        let _ = write_storage_volume(ctx, session_id, &storage_volume);
    }
    let runtime_rootfs = prepare_runtime_rootfs(ctx, session_id, &selected_rootfs)?;
    let launch_rootfs = PathBuf::from(
        runtime_rootfs["rootfs"]
            .as_str()
            .unwrap_or(&selected_rootfs.display().to_string()),
    );
    let command = req["command"]
        .as_array()
        .cloned()
        .ok_or_else(|| "command required".to_string())?;
    let ml_profile = rootfs_is_ml_profile(&selected_rootfs);
    let command_for_launch = if ml_profile {
        if !rootfs_has_ml_bridge(&selected_rootfs) {
            let _ = unmount_workspace_volume(ctx, session_id, false);
            let _ = unmount_runtime_rootfs(ctx, session_id, false);
            return Err("ML profile requires /opt/gema/bin/gema-ml-bridge".to_string());
        }
        ml_bridge_command(&command)
    } else {
        command.clone()
    };
    let ml_recovery_contract = if ml_profile {
        match ensure_ml_recovery_contract(
            ctx,
            session_id,
            Path::new(storage_volume["mountpoint"].as_str().unwrap_or("")),
            &selected_rootfs,
            &command,
        ) {
            Ok(contract) => contract,
            Err(err) => {
                let _ = unmount_workspace_volume(ctx, session_id, false);
                let _ = unmount_runtime_rootfs(ctx, session_id, false);
                return Err(format!(
                    "ML recovery contract required before launch: {err}"
                ));
            }
        }
    } else {
        Value::Null
    };
    let substrate_preflight = match substrate_preflight(
        ctx,
        session_id,
        &launch_rootfs,
        Path::new(storage_volume["mountpoint"].as_str().unwrap_or("")),
        &command_for_launch,
        req,
        &runtime_rootfs,
    ) {
        Ok(preflight) => preflight,
        Err(err) => {
            let _ = unmount_workspace_volume(ctx, session_id, false);
            let _ = unmount_runtime_rootfs(ctx, session_id, false);
            return Err(err);
        }
    };
    let guest_port = req["guest_port"]
        .as_u64()
        .ok_or_else(|| "guest_port required".to_string())? as u16;
    let actor = req["actor"].as_str().unwrap_or("user");
    let protocol = req["protocol"].as_str().unwrap_or("tcp");
    let allocation = allocate_port(
        ctx,
        session_id,
        guest_port,
        protocol,
        req.get("host_port").cloned(),
        req["visibility"].as_str().unwrap_or("public"),
        actor,
    )?;
    let host_port = allocation["host_port"].as_u64().map(|v| v as u16);
    let mut port_map_entries: Vec<String> = Vec::new();
    if allocation["visibility"].as_str() == Some("public") {
        if let Some(p) = host_port {
            port_map_entries.push(format!(
                "{p}:{guest_port}/{}",
                allocation["protocol"].as_str().unwrap_or("tcp")
            ));
        }
    }
    let ssh_keys: Vec<String> = req["ssh_public_keys"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(|value| value.trim().to_string()))
                .filter(|value| !value.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let ssh_allocation = if req.get("ssh_host_port").is_some() {
        let allocation = match allocate_port(
            ctx,
            session_id,
            22,
            "tcp",
            req.get("ssh_host_port").cloned(),
            "public",
            "system",
        ) {
            Ok(allocation) => allocation,
            Err(err) => {
                let _ = release_session_ports(ctx, session_id, "system");
                let _ = unmount_workspace_volume(ctx, session_id, false);
                let _ = unmount_runtime_rootfs(ctx, session_id, false);
                return Err(err);
            }
        };
        if let Some(p) = allocation["host_port"].as_u64().map(|v| v as u16) {
            port_map_entries.push(format!("{p}:22/tcp"));
        }
        Some(allocation)
    } else {
        None
    };
    let port_maps = port_map_entries.join(",");
    let gpu_assignment = read_json(&gpu_assignment_path(ctx, session_id), json!({}));
    let session_dir = ctx.session_dir(session_id);
    fs::create_dir_all(session_dir.join("logs")).map_err(|err| err.to_string())?;
    let _ = rotate_session_logs(ctx, session_id);
    let log_path = session_dir.join("logs/hostd-launch.log");
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|err| err.to_string())?;
    let mut cmd = Command::new(&ctx.substrate);
    cmd.arg("run").arg("--");
    for part in &command_for_launch {
        cmd.arg(part.as_str().unwrap_or(""));
    }
    cmd.env("GEMA_KEEP", "1")
        .env("GEMA_PRESERVE_SESSION", "1")
        .env("GEMA_SESSION_ID", session_id)
        .env(
            "GEMA_USER_ID",
            req["user_id"]
                .as_str()
                .filter(|value| !value.trim().is_empty() && *value != "local")
                .ok_or("launch requires a non-local user_id for CPU storage isolation")?,
        )
        .env("GEMA_GUEST_UID", session_guest_uid(session_id).to_string())
        .env("GEMA_GUEST_GID", session_guest_gid(session_id).to_string())
        .env(
            "GEMA_WORKSPACE",
            storage_volume["mountpoint"].as_str().unwrap_or(""),
        )
        .env(
            "GEMA_AGENT_DIR",
            Path::new(storage_volume["mountpoint"].as_str().unwrap_or(""))
                .join(".gema")
                .join("agent"),
        )
        .env("GEMA_PORT_MAPS", &port_maps)
        .env(
            "GEMA_SSHD_ENABLED",
            if req.get("ssh_host_port").is_some() {
                "1"
            } else {
                "0"
            },
        )
        .env("GEMA_AUTHORIZED_KEYS", ssh_keys.join("\n"))
        .env(
            "GEMA_ISOLATION_LANE",
            policy["lane"].as_str().unwrap_or("standard"),
        )
        .env(
            "GEMA_ROUTER_BIN",
            env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "/root/gema-system/target/debug/gema-runtime".to_string()),
        )
        .env(
            "GEMA_HARDENING",
            req["hardening"]
                .as_str()
                .unwrap_or(if policy["hardening"].as_bool().unwrap_or(true) {
                    "1"
                } else {
                    "0"
                }),
        )
        .env(
            "GEMA_NO_NEW_PRIVS",
            req["no_new_privs"].as_str().unwrap_or(
                if policy["no_new_privs"].as_bool().unwrap_or(true) {
                    "1"
                } else {
                    "0"
                },
            ),
        )
        .env(
            "GEMA_DROP_CAPS",
            req["drop_caps"]
                .as_str()
                .unwrap_or(if policy["drop_caps"].as_bool().unwrap_or(true) {
                    "1"
                } else {
                    "0"
                }),
        )
        .env(
            "GEMA_SECCOMP",
            req["seccomp"]
                .as_str()
                .unwrap_or(if policy["seccomp"].as_bool().unwrap_or(true) {
                    "1"
                } else {
                    "0"
                }),
        )
        .env(
            "GEMA_DEVICE_POLICY",
            req["device_policy"].as_str().unwrap_or(
                if policy["device_policy"].as_bool().unwrap_or(true) {
                    "1"
                } else {
                    "0"
                },
            ),
        )
        .stdout(log_file.try_clone().map_err(|err| err.to_string())?)
        .stderr(log_file)
        .stdin(Stdio::null());
    cmd.env("GEMA_ROOTFS", &launch_rootfs);
    if let Some(remote) = r2_workspace_mount["remote"].as_str() {
        if !remote.trim().is_empty() {
            cmd.env("GEMA_R2_WORKSPACE_REMOTE", remote);
        }
    }
    if let Some(profile) = req["apparmor_profile"]
        .as_str()
        .filter(|v| !v.trim().is_empty())
    {
        cmd.env("GEMA_APPARMOR_PROFILE", profile);
    }
    if gpu_assignment.is_object() && !gpu_assignment.as_object().unwrap().is_empty() {
        if let Some(v) = gpu_assignment["vendor"].as_str() {
            cmd.env("GEMA_GPU_VENDOR", v);
        }
        if let Some(v) = gpu_assignment["device_id"].as_str() {
            cmd.env("GEMA_GPU_DEVICE_ID", v);
            if gpu_assignment["vendor"].as_str() == Some("nvidia") {
                cmd.env("CUDA_VISIBLE_DEVICES", v);
                if let Some(cuda_version) = host_nvidia_cuda_version() {
                    cmd.env("GEMA_HOST_CUDA_VERSION", cuda_version);
                }
                cmd.env(
                    "GEMA_CUDA_LANE",
                    req["cuda_lane"].as_str().unwrap_or("auto"),
                );
            }
        }
        cmd.env(
            "GEMA_GPU_DEVICE_BINDS",
            serde_json::to_string(&gpu_assignment["device_binds"]).unwrap_or_default(),
        );
        cmd.env("GEMA_DEVICE_BINDS", device_binds_env(&gpu_assignment));
    }
    let user_s3_workspace = user_s3_mountpoint(ctx, session_id, "workspace");
    if mounted_at(&user_s3_workspace) {
        cmd.env(
            "GEMA_EXTRA_BINDS",
            format!("{}:/home/gema/external/s3", user_s3_workspace.display()),
        );
    }
    cmd.env(
        "GEMA_DEVICE_ALLOWLIST",
        serde_json::to_string(&device_allowlist(&gpu_assignment))
            .unwrap_or_else(|_| "[]".to_string()),
    );
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = release_session_ports(ctx, session_id, "system");
            let _ = unmount_workspace_volume(ctx, session_id, false);
            let _ = unmount_runtime_rootfs(ctx, session_id, false);
            return Err(format!("spawn substrate: {err}"));
        }
    };
    let pid = child.id() as i64;
    register_process(
        ctx,
        session_id,
        "main",
        pid,
        json!([
            ctx.substrate.display().to_string(),
            "run",
            "--",
            command_for_launch.clone()
        ]),
        req["restart_policy"].as_str().unwrap_or("never"),
        json!(log_path.display().to_string()),
        "substrate",
        json!({"guest_port": guest_port, "host_port": host_port, "port_allocation": allocation, "ssh_port_allocation": ssh_allocation}),
    )?;
    let monitor_ctx = ctx.clone();
    let monitor_session = session_id.to_string();
    thread::spawn(move || monitor_process(monitor_ctx, monitor_session, child));
    let restart_attempts = req["restart_attempt"].as_u64().unwrap_or(0);
    let restart_max_attempts = req["restart_max_attempts"].as_u64().unwrap_or(3);
    let metadata = json!({"session_id": session_id, "host_port": host_port, "guest_port": guest_port, "port_allocation": allocation, "ssh_port_allocation": ssh_allocation, "isolation": policy, "preflight": host_preflight, "launch_readiness": launch_readiness, "substrate_preflight": substrate_preflight, "storage_volume": storage_volume, "r2_workspace_mount": r2_workspace_mount, "gpu_assignment": gpu_assignment, "base_rootfs": selected_rootfs.display().to_string(), "rootfs": launch_rootfs.display().to_string(), "runtime_rootfs": runtime_rootfs, "ml_recovery_contract": ml_recovery_contract, "fabric_bridge": if ml_profile { json!({"enabled": true, "entrypoint": "/opt/gema/bin/gema-fabric-bridge", "compat_entrypoint": "/opt/gema/bin/gema-ml-bridge", "schema": "gema.fabric_bridge.v1"}) } else { Value::Null }, "ml_bridge": if ml_profile { json!({"enabled": true, "entrypoint": "/opt/gema/bin/gema-ml-bridge", "command": command_for_launch}) } else { Value::Null }, "command": command, "pid": pid, "restart_policy": req["restart_policy"].as_str().unwrap_or("never"), "restart_attempts": restart_attempts, "restart_max_attempts": restart_max_attempts, "launch_request": req, "started_at": now()});
    write_json_atomic(&session_dir.join("hostd.json"), &metadata)?;
    let _ = write_session_port_allocations(ctx, session_id, &load_port_allocations(ctx));
    let _ = collect_storage(ctx, session_id, true);
    let _ = scan_session_processes(ctx, session_id);
    transition(
        ctx,
        session_id,
        "queued",
        "launch_requested",
        json!({"host_port": host_port, "guest_port": guest_port}),
        false,
    )?;
    transition(
        ctx,
        session_id,
        "preparing",
        "preparing_substrate",
        json!({}),
        true,
    )?;
    transition(
        ctx,
        session_id,
        "starting",
        "substrate_process_spawn",
        json!({}),
        true,
    )?;
    log(
        ctx,
        &format!(
            "launch session={session_id} pid={pid} port={:?}->{guest_port}",
            host_port
        ),
    );
    append_runtime_event(
        ctx,
        session_id,
        "session_launched",
        json!({"pid": pid, "host_port": host_port, "guest_port": guest_port, "command": metadata["command"].clone(), "preflight": metadata["preflight"].clone(), "storage_volume": metadata["storage_volume"].clone()}),
        "system",
        "info",
    )?;
    append_runtime_event(
        ctx,
        session_id,
        "user_command",
        json!({"command": metadata["command"].clone(), "source": "launch"}),
        actor,
        "info",
    )?;
    append_runtime_event(
        ctx,
        session_id,
        "terminal_command",
        json!({"command": metadata["command"].clone(), "cwd": "/home/gema/workspace", "source": "launch"}),
        actor,
        "info",
    )?;
    let startup_timeout =
        Duration::from_secs(req["startup_timeout_seconds"].as_u64().unwrap_or(120));
    let startup_deadline = Instant::now() + startup_timeout;
    while Instant::now() < startup_deadline
        && pid_alive(pid)
        && !session_has_live_workload(ctx, session_id)
    {
        let _ = scan_session_processes(ctx, session_id);
        thread::sleep(Duration::from_millis(200));
    }
    if !session_has_live_workload(ctx, session_id) {
        transition(
            ctx,
            session_id,
            "failed",
            "no_live_workload_after_start",
            json!({"host_port": host_port}),
            false,
        )?;
        cleanup_runtime_preserve(ctx, session_id)?;
        release_session_ports(ctx, session_id, "system")?;
        return Err("no live workload after start".to_string());
    } else if let Some(port) = host_port {
        if protocol == "udp" {
            let _ = scan_session_processes(ctx, session_id);
            transition(
                ctx,
                session_id,
                "running",
                "udp_router_started",
                json!({"host_port": port}),
                false,
            )?;
        } else if wait_for_service_ready(
            ctx,
            session_id,
            guest_port,
            req,
            Duration::from_secs(req["service_timeout_seconds"].as_u64().unwrap_or(120)),
        ) {
            let _ = scan_session_processes(ctx, session_id);
            transition(
                ctx,
                session_id,
                "ready",
                if req["health_path"].as_str().is_some() {
                    "healthcheck_ready"
                } else {
                    "guest_port_ready"
                },
                json!({"host_port": port, "guest_port": guest_port, "health_path": req["health_path"].clone()}),
                true,
            )?;
            transition(
                ctx,
                session_id,
                "running",
                "ready_promoted_to_running",
                json!({"host_port": port, "guest_port": guest_port}),
                true,
            )?;
        } else {
            let _ = scan_session_processes(ctx, session_id);
            if req["health_path"].as_str().is_some() {
                transition(
                    ctx,
                    session_id,
                    "failed",
                    "healthcheck_timeout",
                    json!({"host_port": port, "guest_port": guest_port, "health_path": req["health_path"].clone()}),
                    false,
                )?;
                cleanup_runtime_preserve(ctx, session_id)?;
                release_session_ports(ctx, session_id, "system")?;
                return Err("healthcheck timeout".to_string());
            } else {
                transition(
                    ctx,
                    session_id,
                    "running",
                    "process_alive_service_not_ready_yet",
                    json!({"host_port": port, "guest_port": guest_port, "health_path": req["health_path"].clone()}),
                    false,
                )?;
            }
        }
    } else {
        let _ = scan_session_processes(ctx, session_id);
        transition(
            ctx,
            session_id,
            "running",
            "process_alive_private_port",
            json!({}),
            false,
        )?;
    }
    Ok(
        json!({"ok": true, "pid": pid, "port_allocation": metadata["port_allocation"].clone(), "storage_volume": metadata["storage_volume"].clone(), "preflight": metadata["preflight"].clone()}),
    )
}

fn cleanup(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let session_id = req["session_id"]
        .as_str()
        .ok_or_else(|| "session_id required".to_string())?;
    let delete = req["delete"].as_bool().unwrap_or(false);
    let _ = scan_session_processes(ctx, session_id);
    fs::create_dir_all(ctx.session_dir(session_id)).map_err(|err| err.to_string())?;
    fs::write(ctx.session_dir(session_id).join(".stop_requested"), now())
        .map_err(|err| err.to_string())?;
    transition(
        ctx,
        session_id,
        "stopping",
        "stop_requested",
        json!({"delete": delete}),
        false,
    )?;
    append_runtime_event(
        ctx,
        session_id,
        "session_stop_requested",
        json!({"delete": delete}),
        req["actor"].as_str().unwrap_or("user"),
        "info",
    )?;
    let out = Command::new(&ctx.substrate)
        .arg("cleanup")
        .arg(session_id)
        .env("GEMA_PRESERVE_SESSION", if delete { "0" } else { "1" })
        .output()
        .map_err(|err| err.to_string())?;
    log(
        ctx,
        &format!(
            "cleanup session={session_id} exit={}",
            out.status.code().unwrap_or(-1)
        ),
    );
    if out.status.success() {
        let registry = read_process_registry(ctx, session_id);
        if let Some(active) = registry["active"].as_object() {
            for record in active.values() {
                if let Some(pid) = record["pid"].as_i64() {
                    let _ = finish_process(
                        ctx,
                        session_id,
                        record["kind"].as_str().unwrap_or("process"),
                        pid,
                        Some(0),
                        Some("session_stopped"),
                        true,
                    );
                }
            }
        }
        let storage_volume = unmount_workspace_volume(ctx, session_id, delete)?;
        let runtime_rootfs = unmount_runtime_rootfs(ctx, session_id, delete)?;
        let released_ports =
            release_session_ports(ctx, session_id, req["actor"].as_str().unwrap_or("user"))?;
        if !delete {
            transition(
                ctx,
                session_id,
                "stopped",
                "stop_completed",
                json!({}),
                false,
            )?;
            append_runtime_event(
                ctx,
                session_id,
                "session_stopped",
                json!({"delete": delete, "released_ports": released_ports, "storage_volume": storage_volume, "runtime_rootfs": runtime_rootfs}),
                "system",
                "info",
            )?;
        }
    } else {
        transition(
            ctx,
            session_id,
            "failed",
            "stop_failed",
            json!({"status": out.status.code(), "stderr": String::from_utf8_lossy(&out.stderr).to_string()}),
            false,
        )?;
        append_runtime_event(
            ctx,
            session_id,
            "session_stop_failed",
            json!({"status": out.status.code(), "stderr": String::from_utf8_lossy(&out.stderr).to_string()}),
            "system",
            "error",
        )?;
    }
    Ok(
        json!({"ok": out.status.success(), "stdout": String::from_utf8_lossy(&out.stdout).to_string(), "stderr": String::from_utf8_lossy(&out.stderr).to_string(), "status": out.status.code()}),
    )
}

fn cleanup_runtime_preserve(ctx: &Ctx, session_id: &str) -> HResult<()> {
    let _ = Command::new(&ctx.substrate)
        .arg("cleanup")
        .arg(session_id)
        .env("GEMA_PRESERVE_SESSION", "1")
        .output();
    let _ = unmount_workspace_volume(ctx, session_id, false);
    let _ = unmount_runtime_rootfs(ctx, session_id, false);
    Ok(())
}

fn inspect(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let id = req["session_id"]
        .as_str()
        .ok_or_else(|| "session_id required".to_string())?;
    let processes = scan_session_processes(ctx, id)?;
    let out = Command::new(&ctx.substrate)
        .arg("inspect")
        .arg(id)
        .output()
        .map_err(|err| err.to_string())?;
    Ok(
        json!({"ok": out.status.success(), "lifecycle": read_json(&lifecycle_path(ctx, id), json!({})), "network": read_json(&ctx.session_dir(id).join("network.json"), json!({})), "metrics": read_json(&ctx.session_dir(id).join("metrics.latest.json"), json!({})), "security": read_json(&ctx.session_dir(id).join("security.json"), json!({})), "storage": read_json(&storage_latest_path(ctx, id), json!({})), "ports": read_json(&session_port_allocations_path(ctx, id), json!({})), "processes": processes, "runtime_events_path": runtime_events_path(ctx, id).display().to_string(), "stdout": String::from_utf8_lossy(&out.stdout).to_string(), "stderr": String::from_utf8_lossy(&out.stderr).to_string(), "status": out.status.code()}),
    )
}

fn logs(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let id = req["session_id"]
        .as_str()
        .ok_or_else(|| "session_id required".to_string())?;
    let lines = req["lines"].as_u64().unwrap_or(200) as usize;
    let dir = ctx.session_dir(id).join("logs");
    let rotated = rotate_session_logs(ctx, id).unwrap_or(0);
    let mut output = String::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                output.push_str(&format!("==> {} <==\n", path.display()));
                let text = fs::read_to_string(&path).unwrap_or_default();
                let tail = text.lines().rev().take(lines).collect::<Vec<_>>();
                for line in tail.into_iter().rev() {
                    output.push_str(line);
                    output.push('\n');
                }
            }
        }
    }
    Ok(
        json!({"ok": true, "stdout": output, "rotation": {"rotated": rotated, "max_bytes": ctx.max_log_bytes, "retention": ctx.log_retention_count}}),
    )
}

fn agent_api_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("agent_api.json")
}

fn agent_audit_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.session_dir(id).join("agent.audit.jsonl")
}

fn agent_workspace_socket_path(ctx: &Ctx, id: &str) -> PathBuf {
    ctx.root
        .join("workspaces")
        .join(id)
        .join("agent")
        .join("hostd.sock")
}

fn agent_api_config(ctx: &Ctx, session_id: &str) -> HResult<Value> {
    let existing = read_json(&agent_api_path(ctx, session_id), Value::Null);
    if existing.is_object() {
        return Ok(existing);
    }
    let config = json!({"session_id": session_id, "transport": "per-session-unix-socket", "host_socket": agent_workspace_socket_path(ctx, session_id).display().to_string(), "guest_socket": "/home/gema/.gema/agent/hostd.sock", "fallback_socket": ctx.socket_path.display().to_string(), "allowed_actions": ["facts", "audit", "snapshot-create", "snapshot-restore", "process-checkpoint-create", "process-checkpoint-restore", "process-checkpoints", "execution-contract", "accelerator-attach", "accelerator-detach", "accelerator-status", "fabric-contract", "fabric-status", "cuda-proxy-contract", "cuda-proxy-status", "r2-sync", "r2-mount", "r2-unmount", "storage", "metrics", "processes", "package-install", "package-list", "package-verify-lock", "package-replay-lock", "gpu"], "created_at": now(), "updated_at": now()});
    write_json_atomic(&agent_api_path(ctx, session_id), &config)?;
    Ok(config)
}

fn agent_facts(ctx: &Ctx, session_id: &str) -> HResult<Value> {
    Ok(
        json!({"session_id": session_id, "lifecycle": read_json(&lifecycle_path(ctx, session_id), json!({})), "metrics": read_json(&ctx.session_dir(session_id).join("metrics.latest.json"), json!({})), "storage": read_json(&storage_latest_path(ctx, session_id), json!({})), "processes": scan_session_processes(ctx, session_id)?, "process_checkpoints": read_process_checkpoint_manifest(ctx, session_id), "snapshots": read_snapshot_manifest(ctx, session_id), "recovery_plan": read_json(&recovery_plan_path(ctx, session_id), json!({})), "execution_contract": read_json(&execution_contract_path(ctx, session_id), json!({})), "fabric": fabric_status(ctx, session_id)?, "accelerator": accelerator_status(ctx, session_id)?, "cuda_proxy": cuda_proxy_status(ctx, session_id)?, "r2": read_r2_state(ctx, session_id), "user_s3": read_user_s3_state(ctx, session_id), "checkpoint_policy": read_checkpoint_policy(ctx, session_id), "persistence": read_json(&persistence_status_path(ctx, session_id), json!({})), "gpu_assignment": read_json(&gpu_assignment_path(ctx, session_id), json!({})), "agent_api": agent_api_config(ctx, session_id)?}),
    )
}

fn agent_api_action(ctx: &Ctx, req: &Value) -> HResult<Value> {
    let session_id = req["session_id"]
        .as_str()
        .ok_or_else(|| "session_id required".to_string())?;
    let action = req["agent_action"].as_str().unwrap_or("facts");
    let details = req["details"].clone();
    let audit =
        json!({"session_id": session_id, "action": action, "details": details, "at": now()});
    append_jsonl(&agent_audit_path(ctx, session_id), &audit)?;
    append_runtime_event(
        ctx,
        session_id,
        "agent_api_call",
        audit.clone(),
        req["actor"].as_str().unwrap_or("agent"),
        "info",
    )?;
    match action {
        "facts" => Ok(json!({"ok": true, "facts": agent_facts(ctx, session_id)?})),
        "audit" => Ok(
            json!({"ok": true, "audit_path": agent_audit_path(ctx, session_id).display().to_string()}),
        ),
        "snapshot-create" => {
            create_local_snapshot(ctx, session_id, details["snapshot_id"].as_str(), "agent")
        }
        "snapshot-restore" => Ok(
            json!({"ok": true, "restore": restore_local_snapshot(ctx, session_id, details["snapshot_id"].as_str(), "agent")?}),
        ),
        "r2-sync" => Ok(
            json!({"ok": true, "sync": r2_sync(ctx, session_id, details["target"].as_str().unwrap_or("workspace"), details["direction"].as_str().unwrap_or("push"))?}),
        ),
        "r2-mount" => Ok(
            json!({"ok": true, "mount": r2_mount(ctx, session_id, details["target"].as_str().unwrap_or("workspace"))?}),
        ),
        "r2-unmount" => Ok(
            json!({"ok": true, "unmount": r2_unmount(ctx, session_id, details["target"].as_str().unwrap_or("workspace"))?}),
        ),
        "storage" => Ok(json!({"ok": true, "storage": collect_storage(ctx, session_id, true)?})),
        "metrics" => Ok(json!({"ok": true, "metrics": collect_metrics(ctx, session_id)?})),
        "processes" => {
            Ok(json!({"ok": true, "processes": scan_session_processes(ctx, session_id)?}))
        }
        "process-checkpoints" => Ok(
            json!({"ok": true, "process_checkpoints": read_process_checkpoint_manifest(ctx, session_id)}),
        ),
        "execution-contract" => Ok(
            json!({"ok": true, "execution_contract": write_execution_contract(ctx, session_id, req)?}),
        ),
        "accelerator-attach" => accelerator_attach(ctx, req),
        "accelerator-detach" => accelerator_detach(ctx, req),
        "accelerator-status" => {
            Ok(json!({"ok": true, "accelerator": accelerator_status(ctx, session_id)?}))
        }
        "fabric-contract" => {
            Ok(json!({"ok": true, "fabric": write_fabric_manifest(ctx, session_id, req)?}))
        }
        "fabric-status" => Ok(json!({"ok": true, "fabric": fabric_status(ctx, session_id)?})),
        "cuda-proxy-contract" => {
            Ok(json!({"ok": true, "cuda_proxy": cuda_proxy_contract(ctx, req)?}))
        }
        "cuda-proxy-status" => {
            Ok(json!({"ok": true, "cuda_proxy": cuda_proxy_status(ctx, session_id)?}))
        }
        "process-checkpoint-create" => process_checkpoint_create(ctx, req),
        "process-checkpoint-restore" => process_checkpoint_restore(ctx, req),
        "package-install" => package_install(ctx, req),
        "package-list" => package_list(ctx, req),
        "package-verify-lock" => package_lock_action(ctx, req, "verify-lock"),
        "package-replay-lock" => package_lock_action(ctx, req, "replay-lock"),
        "gpu" => {
            let assignment = read_json(&gpu_assignment_path(ctx, session_id), json!({}));
            Ok(json!({"ok": true, "gpu_assignment": assignment, "gpu": gpu_metrics(&assignment)}))
        }
        _ => Err(format!("agent action not allowed: {action}")),
    }
}

fn start_agent_socket(ctx: Ctx, session_id: String) -> HResult<Value> {
    let sock_path = agent_workspace_socket_path(&ctx, &session_id);
    if sock_path.exists() {
        let _ = fs::remove_file(&sock_path);
    }
    fs::create_dir_all(sock_path.parent().unwrap()).map_err(|err| err.to_string())?;
    let listener = UnixListener::bind(&sock_path).map_err(|err| err.to_string())?;
    fs::set_permissions(&sock_path, fs::Permissions::from_mode(0o660))
        .map_err(|err| err.to_string())?;
    let config = agent_api_config(&ctx, &session_id)?;
    append_runtime_event(
        &ctx,
        &session_id,
        "agent_socket_ready",
        config.clone(),
        "system",
        "info",
    )?;
    thread::spawn(move || {
        log(
            &ctx,
            &format!(
                "agent socket online session={session_id} socket={}",
                sock_path.display()
            ),
        );
        for stream in listener.incoming().flatten() {
            let c = ctx.clone();
            let id = session_id.clone();
            thread::spawn(move || {
                let response = read_stream_json(stream.try_clone().ok()).and_then(|mut req| {
                    if req.get("agent_action").is_none() && req.get("action").is_some() {
                        req["agent_action"] = req["action"].clone();
                    }
                    req["action"] = json!("agent-api");
                    req["session_id"] = json!(id);
                    agent_api_action(&c, &req)
                });
                let _ = send_stream_response(stream, response);
            });
        }
    });
    Ok(config)
}

fn dispatch(ctx: &Ctx, req: &Value) -> HResult<Value> {
    if let Some(id) = req["session_id"].as_str() {
        require_session_id(id)?;
    }
    match req["action"].as_str().unwrap_or("") {
        "launch" => launch(ctx, req),
        "cleanup" | "stop" => cleanup(ctx, req),
        "inspect" => inspect(ctx, req),
        "logs" => logs(ctx, req),
        "event" => {
            let id = req["session_id"]
                .as_str()
                .ok_or_else(|| "session_id required".to_string())?;
            Ok(
                json!({"ok": true, "event": append_runtime_event(ctx, id, req["event_type"].as_str().unwrap_or("event"), req["details"].clone(), req["actor"].as_str().unwrap_or("system"), req["severity"].as_str().unwrap_or("info"))?}),
            )
        }
        "snapshot" => {
            let id = req["session_id"]
                .as_str()
                .ok_or_else(|| "session_id required".to_string())?;
            let status = req["status"].as_str().unwrap_or("");
            if !matches!(status, "started" | "completed" | "failed") {
                return Err("snapshot status must be started, completed, or failed".to_string());
            }
            let entry = record_snapshot_size(ctx, id, status, req["details"].clone())?;
            let storage = collect_storage(ctx, id, true)?;
            Ok(
                json!({"ok": true, "event": append_runtime_event(ctx, id, &format!("snapshot_{status}"), json!({"snapshot": entry, "storage": storage}), req["actor"].as_str().unwrap_or("system"), if status == "failed" { "error" } else { "info" })?, "snapshot": entry, "storage": storage}),
            )
        }
        "snapshot-create" => create_local_snapshot(
            ctx,
            req["session_id"].as_str().ok_or("session_id required")?,
            req["snapshot_id"].as_str(),
            req["actor"].as_str().unwrap_or("system"),
        ),
        "snapshot-restore" => Ok(
            json!({"ok": true, "restore": restore_local_snapshot(ctx, req["session_id"].as_str().ok_or("session_id required")?, req["snapshot_id"].as_str(), req["actor"].as_str().unwrap_or("system"))?}),
        ),
        "recovery-plan" => Ok(
            json!({"ok": true, "recovery_plan": write_recovery_plan(ctx, req["session_id"].as_str().ok_or("session_id required")?, req["reason"].as_str().unwrap_or("manual"))?}),
        ),
        "execution-contract" => Ok(
            json!({"ok": true, "execution_contract": write_execution_contract(ctx, req["session_id"].as_str().ok_or("session_id required")?, req)?}),
        ),
        "accelerator-attach" => accelerator_attach(ctx, req),
        "accelerator-detach" => accelerator_detach(ctx, req),
        "accelerator-status" => Ok(
            json!({"ok": true, "accelerator": accelerator_status(ctx, req["session_id"].as_str().ok_or("session_id required")?)?}),
        ),
        "fabric-contract" => Ok(
            json!({"ok": true, "fabric": write_fabric_manifest(ctx, req["session_id"].as_str().ok_or("session_id required")?, req)?}),
        ),
        "fabric-status" => Ok(
            json!({"ok": true, "fabric": fabric_status(ctx, req["session_id"].as_str().ok_or("session_id required")?)?}),
        ),
        "cuda-proxy-contract" => {
            Ok(json!({"ok": true, "cuda_proxy": cuda_proxy_contract(ctx, req)?}))
        }
        "cuda-proxy-status" => Ok(
            json!({"ok": true, "cuda_proxy": cuda_proxy_status(ctx, req["session_id"].as_str().ok_or("session_id required")?)?}),
        ),
        "r2-configure" => Ok(
            json!({"ok": true, "r2": configure_r2(ctx, req["session_id"].as_str().ok_or("session_id required")?, req["remote"].as_str(), req["prefix"].as_str(), req["enabled"].as_bool().unwrap_or(true))?}),
        ),
        "r2-status" => Ok(
            json!({"ok": true, "r2": read_r2_state(ctx, req["session_id"].as_str().ok_or("session_id required")?), "probe": if req["probe"].as_bool().unwrap_or(false) { r2_probe(ctx, req["session_id"].as_str().unwrap())? } else { Value::Null }}),
        ),
        "r2-sync" => Ok(
            json!({"ok": true, "sync": r2_sync(ctx, req["session_id"].as_str().ok_or("session_id required")?, req["target"].as_str().unwrap_or("workspace"), req["direction"].as_str().unwrap_or("push"))?}),
        ),
        "r2-mount" => Ok(
            json!({"ok": true, "mount": r2_mount(ctx, req["session_id"].as_str().ok_or("session_id required")?, req["target"].as_str().unwrap_or("workspace"))?}),
        ),
        "r2-unmount" => Ok(
            json!({"ok": true, "unmount": r2_unmount(ctx, req["session_id"].as_str().ok_or("session_id required")?, req["target"].as_str().unwrap_or("workspace"))?}),
        ),
        "user-s3-configure" | "user-s3-status" | "user-s3-sync" | "user-s3-mount"
        | "user-s3-unmount" => {
            Err("user-s3 storage backend is disabled; managed R2 is required".to_string())
        }
        "checkpoint-policy" => {
            Ok(json!({"ok": true, "checkpoint_policy": configure_checkpoint_policy(ctx, req)?}))
        }
        "persistence-policy" => persistence_policy_action(ctx, req),
        "persistence-status" => Ok(
            json!({"ok": true, "persistence": build_persistence_status(ctx, req["session_id"].as_str().ok_or("session_id required")?, false)?}),
        ),
        "persistence-verify" => Ok(
            json!({"ok": true, "persistence": build_persistence_status(ctx, req["session_id"].as_str().ok_or("session_id required")?, true)?}),
        ),
        "agent-socket" => Ok(
            json!({"ok": true, "agent_api": start_agent_socket(ctx.clone(), req["session_id"].as_str().ok_or("session_id required")?.to_string())?}),
        ),
        "agent-api" => agent_api_action(ctx, req),
        "assign-gpu" => Ok(
            json!({"ok": true, "gpu_assignment": assign_gpu(ctx, req["session_id"].as_str().ok_or("session_id required")?, req["vendor"].as_str().unwrap_or("nvidia"), req["device_id"].as_str().unwrap_or("0"), req["mode"].as_str().unwrap_or("exclusive"), req["actor"].as_str().unwrap_or("system"))?}),
        ),
        "gpu-status" => {
            let assignment = read_json(
                &gpu_assignment_path(
                    ctx,
                    req["session_id"].as_str().ok_or("session_id required")?,
                ),
                json!({}),
            );
            Ok(
                json!({"ok": true, "gpu_assignment": assignment, "metrics": gpu_metrics(&assignment)}),
            )
        }
        "metrics" => Ok(
            json!({"ok": true, "metrics": collect_metrics(ctx, req["session_id"].as_str().ok_or("session_id required")?)?}),
        ),
        "storage" => Ok(
            json!({"ok": true, "storage": collect_storage(ctx, req["session_id"].as_str().ok_or("session_id required")?, true)?}),
        ),
        "set-storage-quota" => {
            let id = req["session_id"].as_str().ok_or("session_id required")?;
            let policy = set_storage_policy(
                ctx,
                id,
                req["quota_bytes"].as_u64(),
                req["snapshot_retention_count"].as_u64(),
                req.get("cleanup_policy").cloned(),
            )?;
            Ok(json!({"ok": true, "policy": policy, "storage": collect_storage(ctx, id, true)?}))
        }
        "cleanup-storage" => Ok(
            json!({"ok": true, "cleanup": cleanup_storage(ctx, req["session_id"].as_str().ok_or("session_id required")?)?}),
        ),
        "mount-storage" => Err(
            "local workspace volume allocation is disabled; managed R2 is required".to_string(),
        ),
        "unmount-storage" => Ok(
            json!({"ok": true, "volume": unmount_workspace_volume(ctx, req["session_id"].as_str().ok_or("session_id required")?, req["delete"].as_bool().unwrap_or(false))?}),
        ),
        "launch-readiness" => build_launch_readiness(ctx, req, true),
        "host-probe" => host_probe(ctx, req),
        "ports" => port_status(ctx),
        "allocate-port" => Ok(
            json!({"ok": true, "allocation": allocate_port(ctx, req["session_id"].as_str().ok_or("session_id required")?, req["guest_port"].as_u64().ok_or("guest_port required")? as u16, req["protocol"].as_str().unwrap_or("tcp"), req.get("host_port").cloned(), req["visibility"].as_str().unwrap_or("public"), req["actor"].as_str().unwrap_or("user"))?}),
        ),
        "release-ports" => Ok(
            json!({"ok": true, "released": release_session_ports(ctx, req["session_id"].as_str().ok_or("session_id required")?, req["actor"].as_str().unwrap_or("user"))?}),
        ),
        "processes" => Ok(
            json!({"ok": true, "processes": scan_session_processes(ctx, req["session_id"].as_str().ok_or("session_id required")?)?}),
        ),
        "process-checkpoints" => Ok(
            json!({"ok": true, "process_checkpoints": read_process_checkpoint_manifest(ctx, req["session_id"].as_str().ok_or("session_id required")?)}),
        ),
        "process-checkpoint-create" => process_checkpoint_create(ctx, req),
        "process-checkpoint-restore" => process_checkpoint_restore(ctx, req),
        "package-install" => package_install(ctx, req),
        "package-list" => package_list(ctx, req),
        "package-verify-lock" => package_lock_action(ctx, req, "verify-lock"),
        "package-replay-lock" => package_lock_action(ctx, req, "replay-lock"),
        "images" => image_catalog(ctx),
        "image-compat" => image_compatibility(ctx, req),
        "image-verify" => image_verify(ctx, req),
        "image-pull" => image_pull(ctx, req),
        "image-prune" => image_prune(ctx, req),
        "status" => Ok(
            json!({"ok": true, "pid": std::process::id(), "socket": ctx.socket_path.display().to_string()}),
        ),
        "shutdown" => Ok(json!({"ok": true, "shutdown": true})),
        action => Err(format!("unknown action: {action}")),
    }
}

fn port_status(ctx: &Ctx) -> HResult<Value> {
    let allocations = load_port_allocations(ctx);
    let mut available = 0u64;
    for port in ctx.port_pool_start..=ctx.port_pool_end {
        if validate_host_port("tcp", port, &allocations, None).is_none() {
            available += 1;
        }
    }
    Ok(
        json!({"ok": true, "pool": {"start": ctx.port_pool_start, "end": ctx.port_pool_end, "available_tcp": available}, "reserved_platform_ports": reserved_ports().into_iter().collect::<Vec<_>>(), "allocations": allocations["allocations"].clone(), "audit_path": ctx.port_audit_path.display().to_string()}),
    )
}

fn read_stream_json(stream: Option<UnixStream>) -> HResult<Value> {
    let mut stream = stream.ok_or_else(|| "stream unavailable".to_string())?;
    let mut buf = String::new();
    stream
        .read_to_string(&mut buf)
        .map_err(|err| err.to_string())?;
    if buf.trim().is_empty() {
        Ok(json!({}))
    } else {
        serde_json::from_str(&buf).map_err(|err| err.to_string())
    }
}

fn send_stream_response(mut stream: UnixStream, response: HResult<Value>) -> HResult<()> {
    let payload = match response {
        Ok(value) => value,
        Err(err) => json!({"ok": false, "error": err}),
    };
    stream
        .write_all(
            serde_json::to_string(&payload)
                .map_err(|err| err.to_string())?
                .as_bytes(),
        )
        .map_err(|err| err.to_string())?;
    stream.write_all(b"\n").map_err(|err| err.to_string())?;
    Ok(())
}

fn run_tcp_router(host_port: u16, guest_ip: &str, guest_port: u16) -> HResult<()> {
    let listener = TcpListener::bind(("0.0.0.0", host_port))
        .map_err(|err| format!("bind tcp/{host_port}: {err}"))?;
    println!(
        "{} router tcp listen=0.0.0.0:{host_port} upstream={guest_ip}:{guest_port}",
        now()
    );
    for incoming in listener.incoming() {
        match incoming {
            Ok(mut client) => {
                let upstream = format!("{guest_ip}:{guest_port}");
                thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_millis(1500);
                    let mut server = loop {
                        match TcpStream::connect(&upstream) {
                            Ok(stream) => break stream,
                            Err(err) if Instant::now() < deadline => {
                                eprintln!(
                                    "{} router tcp upstream_retry upstream={} error={}",
                                    now(),
                                    upstream,
                                    err
                                );
                                thread::sleep(Duration::from_millis(200));
                            }
                            Err(err) => {
                                eprintln!(
                                    "{} router tcp upstream_failed upstream={} error={}",
                                    now(),
                                    upstream,
                                    err
                                );
                                return;
                            }
                        }
                    };
                    let mut client_read = match client.try_clone() {
                        Ok(v) => v,
                        Err(err) => {
                            eprintln!("{} router tcp client_clone_failed error={}", now(), err);
                            return;
                        }
                    };
                    let mut server_write = match server.try_clone() {
                        Ok(v) => v,
                        Err(err) => {
                            eprintln!("{} router tcp server_clone_failed error={}", now(), err);
                            return;
                        }
                    };
                    let upstream_to_client = thread::spawn(move || {
                        let _ = std::io::copy(&mut server, &mut client);
                    });
                    let _ = std::io::copy(&mut client_read, &mut server_write);
                    let _ = upstream_to_client.join();
                });
            }
            Err(err) => eprintln!("{} router tcp accept_error error={}", now(), err),
        }
    }
    Ok(())
}

fn run_udp_router(host_port: u16, guest_ip: &str, guest_port: u16) -> HResult<()> {
    let socket = UdpSocket::bind(("0.0.0.0", host_port))
        .map_err(|err| format!("bind udp/{host_port}: {err}"))?;
    socket.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let upstream = format!("{guest_ip}:{guest_port}");
    let upstream_socket = UdpSocket::bind(("0.0.0.0", 0))
        .map_err(|err| format!("bind udp upstream socket: {err}"))?;
    upstream_socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok();
    println!(
        "{} router udp listen=0.0.0.0:{host_port} upstream={upstream}",
        now()
    );
    let mut buf = vec![0u8; 65535];
    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, peer)) => {
                if let Err(err) = upstream_socket.send_to(&buf[..len], &upstream) {
                    eprintln!(
                        "{} router udp upstream_send_failed peer={} error={}",
                        now(),
                        peer,
                        err
                    );
                    continue;
                }
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    let mut response = vec![0u8; 65535];
                    match upstream_socket.recv_from(&mut response) {
                        Ok((n, _)) => {
                            let _ = socket.send_to(&response[..n], peer);
                            break;
                        }
                        Err(err)
                            if matches!(
                                err.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) && Instant::now() < deadline =>
                        {
                            continue;
                        }
                        Err(err) => {
                            eprintln!(
                                "{} router udp upstream_recv_failed peer={} error={}",
                                now(),
                                peer,
                                err
                            );
                            break;
                        }
                    }
                }
            }
            Err(err) => eprintln!("{} router udp recv_error error={}", now(), err),
        }
    }
}

fn run_router(args: &[String]) -> HResult<ExitCode> {
    if args.len() < 4 {
        eprintln!("usage: gema-runtime hostd router <tcp|udp> <host_port> <guest_ip> <guest_port>");
        return Ok(ExitCode::from(64));
    }
    let protocol = normalize_protocol(&args[0]);
    let host_port = args[1]
        .parse::<u16>()
        .map_err(|_| "invalid host_port".to_string())?;
    let guest_ip = &args[2];
    let guest_port = args[3]
        .parse::<u16>()
        .map_err(|_| "invalid guest_port".to_string())?;
    match protocol.as_str() {
        "tcp" => run_tcp_router(host_port, guest_ip, guest_port)?,
        "udp" => run_udp_router(host_port, guest_ip, guest_port)?,
        _ => return Err("protocol must be tcp or udp".to_string()),
    }
    Ok(ExitCode::SUCCESS)
}

fn seccomp_stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn seccomp_jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

fn install_gema_seccomp_filter() -> HResult<()> {
    const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const BPF_LD_W_ABS: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    const BPF_JMP_JEQ_K: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    const BPF_RET_K: u16 = (libc::BPF_RET | libc::BPF_K) as u16;
    let errno = SECCOMP_RET_ERRNO | (libc::EPERM as u32);
    let denied = [
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_ptrace,
        libc::SYS_kexec_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_open_by_handle_at,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_iopl,
        libc::SYS_ioperm,
        libc::SYS_acct,
        libc::SYS_setns,
        libc::SYS_unshare,
    ];
    let mut filter = vec![
        seccomp_stmt(BPF_LD_W_ABS, 4),
        seccomp_jump(BPF_JMP_JEQ_K, AUDIT_ARCH_X86_64, 1, 0),
        seccomp_stmt(BPF_RET_K, SECCOMP_RET_KILL_PROCESS),
        seccomp_stmt(BPF_LD_W_ABS, 0),
    ];
    for syscall in denied {
        filter.push(seccomp_jump(BPF_JMP_JEQ_K, syscall as u32, 0, 1));
        filter.push(seccomp_stmt(BPF_RET_K, errno));
    }
    filter.push(seccomp_stmt(BPF_RET_K, SECCOMP_RET_ALLOW));
    let mut program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    let nnp = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if nnp != 0 {
        return Err(format!(
            "PR_SET_NO_NEW_PRIVS failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let status = unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &mut program as *mut libc::sock_fprog,
            0,
            0,
        )
    };
    if status != 0 {
        return Err(format!(
            "PR_SET_SECCOMP failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn run_seccomp_exec(args: &[String]) -> HResult<ExitCode> {
    if args.first().map(String::as_str) != Some("--") || args.len() < 2 {
        eprintln!("usage: gema-runtime hostd seccomp-exec -- <command...>");
        return Ok(ExitCode::from(64));
    }
    install_gema_seccomp_filter()?;
    let err = Command::new(&args[1]).args(&args[2..]).exec();
    Err(format!("exec failed: {err}"))
}

fn handle_connection(ctx: Ctx, stream: UnixStream) {
    let cloned = stream.try_clone().ok();
    let req = read_stream_json(cloned);
    let mut shutdown = false;
    let response = req.and_then(|req| {
        shutdown = req["action"].as_str() == Some("shutdown");
        dispatch(&ctx, &req)
    });
    let _ = send_stream_response(stream, response);
    if shutdown {
        std::process::exit(0);
    }
}

fn serve(ctx: &Ctx) -> HResult<()> {
    ctx.setup()?;
    let _ = reconcile_sessions(ctx);
    {
        let c = ctx.clone();
        thread::spawn(move || metrics_loop(c));
    }
    {
        let c = ctx.clone();
        thread::spawn(move || checkpoint_loop(c));
    }
    if ctx.socket_path.exists() {
        fs::remove_file(&ctx.socket_path).map_err(|err| err.to_string())?;
    }
    let listener = UnixListener::bind(&ctx.socket_path)
        .map_err(|err| format!("bind {}: {err}", ctx.socket_path.display()))?;
    fs::set_permissions(&ctx.socket_path, fs::Permissions::from_mode(0o600))
        .map_err(|err| err.to_string())?;
    fs::write(&ctx.pid_file, std::process::id().to_string()).map_err(|err| err.to_string())?;
    log(
        ctx,
        &format!(
            "online pid={} socket={}",
            std::process::id(),
            ctx.socket_path.display()
        ),
    );
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let c = ctx.clone();
                thread::spawn(move || handle_connection(c, stream));
            }
            Err(err) => log(ctx, &format!("accept_error error={err}")),
        }
    }
    Ok(())
}

fn client(ctx: &Ctx, payload: &Value) -> HResult<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(&ctx.socket_path) {
            Ok(mut stream) => {
                stream
                    .write_all(
                        serde_json::to_string(payload)
                            .map_err(|err| err.to_string())?
                            .as_bytes(),
                    )
                    .map_err(|err| err.to_string())?;
                stream.shutdown(std::net::Shutdown::Write).ok();
                let mut response = String::new();
                stream
                    .read_to_string(&mut response)
                    .map_err(|err| err.to_string())?;
                print!("{response}");
                if serde_json::from_str::<Value>(&response)
                    .ok()
                    .and_then(|v| v["ok"].as_bool())
                    == Some(false)
                {
                    return Err("request failed".to_string());
                }
                return Ok(());
            }
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                thread::sleep(Duration::from_millis(200));
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

fn parse_json_arg(value: Option<&String>) -> Value {
    value
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_else(|| value.map_or(json!({}), |v| json!({"raw": v})))
}

fn parse_launch_args(args: &[String]) -> HResult<Value> {
    let sep = args.iter().position(|v| v == "--").ok_or_else(|| {
        "usage: launch <session_id> <host_port|auto> <guest_port> [tcp|udp] [--health-path <path>] [--startup-timeout <seconds>] [--service-timeout <seconds>] [--health-timeout-ms <ms>] [--restart-policy <never|on-failure|always>] [--restart-max <count>] [--profile <id>] -- <command...>".to_string()
    })?;
    if sep < 3 || sep + 1 >= args.len() {
        return Err("launch requires session_id, host_port, guest_port, and command".to_string());
    }
    let mut payload = json!({
        "action": "launch",
        "session_id": args.get(0),
        "host_port": args.get(1).map(String::as_str).unwrap_or("auto"),
        "guest_port": args.get(2).and_then(|v| v.parse::<u16>().ok()),
        "protocol": "tcp",
        "user_id": env::var("GEMA_USER_ID").ok().filter(|value| !value.trim().is_empty()),
        "command": args[sep + 1..].to_vec(),
    });
    let mut index = 3;
    while index < sep {
        match args[index].as_str() {
            "tcp" | "udp" => {
                payload["protocol"] = json!(args[index].as_str());
                index += 1;
            }
            "--health-path" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--health-path requires a value".to_string())?;
                payload["health_path"] = json!(value);
                index += 1;
            }
            "--startup-timeout" | "--startup-timeout-seconds" => {
                index += 1;
                let value = args
                    .get(index)
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or_else(|| "--startup-timeout requires seconds".to_string())?;
                payload["startup_timeout_seconds"] = json!(value);
                index += 1;
            }
            "--health-timeout-ms" => {
                index += 1;
                let value = args
                    .get(index)
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or_else(|| "--health-timeout-ms requires milliseconds".to_string())?;
                payload["health_timeout_ms"] = json!(value);
                index += 1;
            }
            "--service-timeout" | "--service-timeout-seconds" => {
                index += 1;
                let value = args
                    .get(index)
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or_else(|| "--service-timeout requires seconds".to_string())?;
                payload["service_timeout_seconds"] = json!(value);
                index += 1;
            }
            "--restart-policy" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--restart-policy requires a value".to_string())?;
                if !matches!(value.as_str(), "never" | "on-failure" | "always") {
                    return Err("restart policy must be never, on-failure, or always".to_string());
                }
                payload["restart_policy"] = json!(value);
                index += 1;
            }
            "--restart-max" | "--restart-max-attempts" => {
                index += 1;
                let value = args
                    .get(index)
                    .and_then(|v| v.parse::<u64>().ok())
                    .ok_or_else(|| "--restart-max requires a count".to_string())?;
                payload["restart_max_attempts"] = json!(value);
                index += 1;
            }
            "--profile" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--profile requires a value".to_string())?;
                payload["profile"] = json!(value);
                index += 1;
            }
            "--user-id" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--user-id requires a value".to_string())?;
                payload["user_id"] = json!(value);
                index += 1;
            }
            "--rootfs" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--rootfs requires a value".to_string())?;
                payload["rootfs"] = json!(value);
                index += 1;
            }
            other => return Err(format!("unknown launch option: {other}")),
        }
    }
    Ok(payload)
}

fn usage() -> &'static str {
    "usage: gema-runtime hostd serve|router|seccomp-exec|status|shutdown|inspect|logs|event|snapshot|snapshot-create|snapshot-restore|process-checkpoint-create|process-checkpoint-restore|process-checkpoints|recovery-plan|execution-contract|accelerator-attach|accelerator-detach|accelerator-status|fabric-contract|fabric-status|cuda-proxy-contract|cuda-proxy-status|r2-configure|r2-status|r2-sync|r2-mount|r2-unmount|user-s3-configure|user-s3-status|user-s3-sync|user-s3-mount|user-s3-unmount|checkpoint-policy|persistence-policy|persistence-status|persistence-verify|agent-socket|agent-api|assign-gpu|gpu-status|metrics|storage|set-storage-quota|cleanup-storage|mount-storage|unmount-storage|launch-readiness|host-probe|ports|allocate-port|release-ports|processes|package-install|package-list|package-verify-lock|package-replay-lock|images|image-compat|image-verify|image-pull|image-prune|cleanup|stop|launch"
}

fn run_cli(ctx: &Ctx, args: Vec<String>) -> HResult<ExitCode> {
    if args.is_empty() {
        eprintln!("{}", usage());
        return Ok(ExitCode::from(64));
    }
    let action = args[0].as_str();
    if action == "serve" {
        serve(ctx)?;
        return Ok(ExitCode::SUCCESS);
    }
    if action == "router" {
        return run_router(&args[1..]);
    }
    if action == "seccomp-exec" {
        return run_seccomp_exec(&args[1..]);
    }
    let payload = match action {
        "status" => json!({"action": "status"}),
        "shutdown" => json!({"action": "shutdown"}),
        "cleanup" => json!({"action": "cleanup", "session_id": args.get(1), "delete": true}),
        "stop" => json!({"action": "stop", "session_id": args.get(1)}),
        "inspect" => json!({"action": "inspect", "session_id": args.get(1)}),
        "logs" => {
            json!({"action": "logs", "session_id": args.get(1), "lines": args.get(2).and_then(|v| v.parse::<u64>().ok()).unwrap_or(200)})
        }
        "event" => {
            json!({"action": "event", "session_id": args.get(1), "event_type": args.get(2), "details": parse_json_arg(args.get(3)), "actor": args.get(4).map(String::as_str).unwrap_or("system"), "severity": args.get(5).map(String::as_str).unwrap_or("info")})
        }
        "snapshot" => {
            json!({"action": "snapshot", "session_id": args.get(1), "status": args.get(2), "details": parse_json_arg(args.get(3))})
        }
        "snapshot-create" => {
            json!({"action": "snapshot-create", "session_id": args.get(1), "snapshot_id": args.get(2)})
        }
        "snapshot-restore" => {
            json!({"action": "snapshot-restore", "session_id": args.get(1), "snapshot_id": args.get(2)})
        }
        "process-checkpoint-create" => {
            json!({"action": "process-checkpoint-create", "session_id": args.get(1), "checkpoint_id": args.get(2), "pid": args.get(3).and_then(|v| v.parse::<i64>().ok())})
        }
        "process-checkpoint-restore" => {
            let mode = args.get(3).map(String::as_str).unwrap_or("");
            json!({"action": "process-checkpoint-restore", "session_id": args.get(1), "checkpoint_id": args.get(2), "terminate_existing": mode == "terminate-existing", "allow_live_restore": mode == "allow-live"})
        }
        "process-checkpoints" => {
            json!({"action": "process-checkpoints", "session_id": args.get(1)})
        }
        "recovery-plan" => json!({"action": "recovery-plan", "session_id": args.get(1)}),
        "execution-contract" => {
            json!({"action": "execution-contract", "session_id": args.get(1), "details": parse_json_arg(args.get(2))})
        }
        "accelerator-attach" => {
            json!({"action": "accelerator-attach", "session_id": args.get(1), "details": parse_json_arg(args.get(2))})
        }
        "accelerator-detach" => {
            json!({"action": "accelerator-detach", "session_id": args.get(1), "mode": args.get(2).map(String::as_str).unwrap_or("drain"), "details": parse_json_arg(args.get(3))})
        }
        "accelerator-status" => json!({"action": "accelerator-status", "session_id": args.get(1)}),
        "fabric-contract" => {
            json!({"action": "fabric-contract", "session_id": args.get(1), "details": parse_json_arg(args.get(2))})
        }
        "fabric-status" => json!({"action": "fabric-status", "session_id": args.get(1)}),
        "cuda-proxy-contract" => {
            json!({"action": "cuda-proxy-contract", "session_id": args.get(1), "details": parse_json_arg(args.get(2))})
        }
        "cuda-proxy-status" => json!({"action": "cuda-proxy-status", "session_id": args.get(1)}),
        "r2-configure" => {
            json!({"action": "r2-configure", "session_id": args.get(1), "remote": args.get(2), "prefix": args.get(3)})
        }
        "r2-status" => {
            json!({"action": "r2-status", "session_id": args.get(1), "probe": args.get(2).is_some()})
        }
        "r2-sync" => {
            json!({"action": "r2-sync", "session_id": args.get(1), "target": args.get(2).map(String::as_str).unwrap_or("workspace"), "direction": args.get(3).map(String::as_str).unwrap_or("push")})
        }
        "r2-mount" => {
            json!({"action": "r2-mount", "session_id": args.get(1), "target": args.get(2).map(String::as_str).unwrap_or("workspace")})
        }
        "r2-unmount" => {
            json!({"action": "r2-unmount", "session_id": args.get(1), "target": args.get(2).map(String::as_str).unwrap_or("workspace")})
        }
        "user-s3-status" => {
            json!({"action": "user-s3-status", "session_id": args.get(1), "probe": args.get(2).is_some()})
        }
        "user-s3-sync" => {
            json!({"action": "user-s3-sync", "session_id": args.get(1), "target": args.get(2).map(String::as_str).unwrap_or("workspace"), "direction": args.get(3).map(String::as_str).unwrap_or("push")})
        }
        "user-s3-mount" => {
            json!({"action": "user-s3-mount", "session_id": args.get(1), "target": args.get(2).map(String::as_str).unwrap_or("workspace")})
        }
        "user-s3-unmount" => {
            json!({"action": "user-s3-unmount", "session_id": args.get(1), "target": args.get(2).map(String::as_str).unwrap_or("workspace")})
        }
        "checkpoint-policy" => {
            json!({"action": "checkpoint-policy", "session_id": args.get(1), "backend": args.get(2).map(String::as_str).unwrap_or("managed"), "interval_seconds": args.get(3).and_then(|v| v.parse::<u64>().ok()).unwrap_or(300)})
        }
        "persistence-policy" => {
            json!({"action": "persistence-policy", "session_id": args.get(1), "backend": args.get(2).map(String::as_str).unwrap_or("managed"), "interval_seconds": args.get(3).and_then(|v| v.parse::<u64>().ok()).unwrap_or(300)})
        }
        "persistence-status" => {
            json!({"action": "persistence-status", "session_id": args.get(1)})
        }
        "persistence-verify" => {
            json!({"action": "persistence-verify", "session_id": args.get(1)})
        }
        "agent-socket" => json!({"action": "agent-socket", "session_id": args.get(1)}),
        "agent-api" => {
            json!({"action": "agent-api", "session_id": args.get(1), "agent_action": args.get(2).map(String::as_str).unwrap_or("facts"), "details": parse_json_arg(args.get(3))})
        }
        "assign-gpu" => {
            json!({"action": "assign-gpu", "session_id": args.get(1), "vendor": args.get(2).map(String::as_str).unwrap_or("nvidia"), "device_id": args.get(3).map(String::as_str).unwrap_or("0"), "mode": args.get(4).map(String::as_str).unwrap_or("exclusive")})
        }
        "gpu-status" => json!({"action": "gpu-status", "session_id": args.get(1)}),
        "metrics" => json!({"action": "metrics", "session_id": args.get(1)}),
        "storage" => json!({"action": "storage", "session_id": args.get(1)}),
        "set-storage-quota" => {
            json!({"action": "set-storage-quota", "session_id": args.get(1), "quota_bytes": args.get(2).and_then(|v| v.parse::<u64>().ok()), "snapshot_retention_count": args.get(3).and_then(|v| v.parse::<u64>().ok())})
        }
        "cleanup-storage" => json!({"action": "cleanup-storage", "session_id": args.get(1)}),
        "mount-storage" => {
            json!({"action": "mount-storage", "session_id": args.get(1), "quota_bytes": args.get(2).and_then(|v| v.parse::<u64>().ok())})
        }
        "unmount-storage" => {
            json!({"action": "unmount-storage", "session_id": args.get(1), "delete": args.get(2).is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "delete"))})
        }
        "launch-readiness" => {
            let mut payload = parse_launch_args(&args[1..])?;
            payload["action"] = json!("launch-readiness");
            payload
        }
        "host-probe" => {
            json!({"action": "host-probe", "mode": args.get(1).map(String::as_str).unwrap_or("free"), "gpu_vendor": args.get(2), "require_r2": args.get(3).is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "r2"))})
        }
        "ports" => json!({"action": "ports"}),
        "allocate-port" => {
            json!({"action": "allocate-port", "session_id": args.get(1), "guest_port": args.get(2).and_then(|v| v.parse::<u16>().ok()), "visibility": args.get(3).map(String::as_str).unwrap_or("public"), "host_port": args.get(4).map(String::as_str).unwrap_or("auto"), "protocol": args.get(5).map(String::as_str).unwrap_or("tcp")})
        }
        "release-ports" => json!({"action": "release-ports", "session_id": args.get(1)}),
        "processes" => json!({"action": "processes", "session_id": args.get(1)}),
        "package-install" => {
            json!({"action": "package-install", "session_id": args.get(1), "manager": args.get(2), "packages": args.iter().skip(3).cloned().collect::<Vec<_>>()})
        }
        "package-list" => json!({"action": "package-list", "session_id": args.get(1)}),
        "package-verify-lock" => {
            json!({"action": "package-verify-lock", "session_id": args.get(1), "lock": args.get(2).map(String::as_str).unwrap_or("latest")})
        }
        "package-replay-lock" => {
            json!({"action": "package-replay-lock", "session_id": args.get(1), "lock": args.get(2).map(String::as_str).unwrap_or("latest")})
        }
        "images" => json!({"action": "images"}),
        "image-compat" => {
            json!({"action": "image-compat", "profile": if args.get(1).map(String::as_str) == Some("any") { Value::Null } else { json!(args.get(1)) }, "gpu_vendor": args.get(2).map(String::as_str).unwrap_or("cpu"), "runtime": args.get(3).map(String::as_str).unwrap_or("gema-native"), "cuda": args.get(4)})
        }
        "image-verify" => {
            json!({"action": "image-verify", "reference": args.get(1), "profile": args.get(2), "cosign_public_key": env::var("GEMA_IMAGE_COSIGN_PUBLIC_KEY").ok()})
        }
        "image-pull" => {
            json!({"action": "image-pull", "reference": args.get(1), "cosign_public_key": env::var("GEMA_IMAGE_COSIGN_PUBLIC_KEY").ok()})
        }
        "image-prune" => json!({"action": "image-prune"}),
        "launch" => parse_launch_args(&args[1..])?,
        _ => {
            eprintln!("{}", usage());
            return Ok(ExitCode::from(64));
        }
    };
    client(ctx, &payload)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn protocol_and_visibility_normalize_to_safe_defaults() {
        assert_eq!(normalize_protocol("udp"), "udp");
        assert_eq!(normalize_protocol("UDP"), "udp");
        assert_eq!(normalize_protocol("tcp"), "tcp");
        assert_eq!(normalize_protocol("bad"), "tcp");
        assert_eq!(normalize_visibility("private"), "private");
        assert_eq!(normalize_visibility("PRIVATE"), "private");
        assert_eq!(normalize_visibility("public"), "public");
        assert_eq!(normalize_visibility("anything"), "public");
    }

    #[test]
    fn restart_policy_matches_exit_status() {
        assert!(restart_allowed("always", Some(0)));
        assert!(restart_allowed("always", Some(42)));
        assert!(!restart_allowed("on-failure", Some(0)));
        assert!(restart_allowed("on-failure", Some(42)));
        assert!(restart_allowed("on-failure", None));
        assert!(!restart_allowed("never", Some(42)));
    }

    #[test]
    fn image_reference_becomes_safe_cache_name() {
        assert_eq!(
            image_safe_name("ghcr.io/gemacloud/os-ml:latest"),
            "ghcr_io_gemacloud_os_ml_latest"
        );
        assert_eq!(image_safe_name("///busybox:latest///"), "busybox_latest");
    }

    #[test]
    fn r2_remote_requires_bucket_qualified_rclone_path() {
        assert_eq!(
            normalize_r2_remote("r2:gema-cloud-volumes/".to_string()).unwrap(),
            "r2:gema-cloud-volumes"
        );
        assert!(normalize_r2_remote("r2".to_string()).is_err());
        assert!(normalize_r2_remote("r2:".to_string()).is_err());
        assert!(normalize_r2_remote("r2:/".to_string()).is_err());
        assert!(normalize_r2_remote("".to_string()).is_err());
    }

    #[test]
    fn launch_cli_parser_accepts_health_restart_and_profile_options() {
        let args = vec![
            "sess".to_string(),
            "auto".to_string(),
            "8888".to_string(),
            "tcp".to_string(),
            "--health-path".to_string(),
            "/status".to_string(),
            "--startup-timeout".to_string(),
            "12".to_string(),
            "--health-timeout-ms".to_string(),
            "500".to_string(),
            "--service-timeout".to_string(),
            "9".to_string(),
            "--restart-policy".to_string(),
            "on-failure".to_string(),
            "--restart-max".to_string(),
            "2".to_string(),
            "--profile".to_string(),
            "ml".to_string(),
            "--".to_string(),
            "/bin/true".to_string(),
        ];
        let payload = parse_launch_args(&args).unwrap();
        assert_eq!(payload["session_id"], "sess");
        assert_eq!(payload["host_port"], "auto");
        assert_eq!(payload["guest_port"], 8888);
        assert_eq!(payload["protocol"], "tcp");
        assert_eq!(payload["health_path"], "/status");
        assert_eq!(payload["startup_timeout_seconds"], 12);
        assert_eq!(payload["health_timeout_ms"], 500);
        assert_eq!(payload["service_timeout_seconds"], 9);
        assert_eq!(payload["restart_policy"], "on-failure");
        assert_eq!(payload["restart_max_attempts"], 2);
        assert_eq!(payload["profile"], "ml");
        assert_eq!(payload["command"][0], "/bin/true");
    }

    #[test]
    fn readiness_checks_tcp_and_http_success_paths() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 128];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
            }
        });
        assert!(http_addr_ready(
            &addr.to_string(),
            "/health",
            Duration::from_secs(1)
        ));
        handle.join().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let _ = listener.accept();
        });
        assert!(tcp_addr_ready(&addr.to_string(), Duration::from_secs(1)));
        handle.join().unwrap();
    }

    #[test]
    fn log_rotation_keeps_bounded_history() {
        let base = env::temp_dir().join(format!(
            "gema-hostd-rotate-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&base).unwrap();
        let log_path = base.join("hostd-launch.log");
        fs::write(&log_path, b"first").unwrap();
        assert!(rotate_log_file(&log_path, 1, 2).unwrap());
        assert!(base.join("hostd-launch.log.1").exists());
        fs::write(&log_path, b"second").unwrap();
        assert!(rotate_log_file(&log_path, 1, 2).unwrap());
        assert!(base.join("hostd-launch.log.1").exists());
        assert!(base.join("hostd-launch.log.2").exists());
        fs::write(&log_path, b"third").unwrap();
        assert!(rotate_log_file(&log_path, 1, 2).unwrap());
        assert!(base.join("hostd-launch.log.1").exists());
        assert!(base.join("hostd-launch.log.2").exists());
        assert!(!base.join("hostd-launch.log.3").exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn session_ids_are_strictly_bounded_path_atoms() {
        assert!(valid_session_id("abc-123_DEF.456"));
        assert!(valid_session_id("1"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("../escape"));
        assert!(!valid_session_id("a/escape"));
        assert!(!valid_session_id("a..b"));
        assert!(!valid_session_id("-leading"));
        assert!(!valid_session_id("_leading"));
        assert!(!valid_session_id(".leading"));
        assert!(!valid_session_id(&"a".repeat(81)));
    }

    #[test]
    fn accelerator_cli_parser_carries_details() {
        let details = r#"{"cuda_lane":"cu124","require_gpu":true}"#.to_string();
        let args = vec![
            "hostd".to_string(),
            "accelerator-attach".to_string(),
            "sess-ml".to_string(),
            details,
        ];
        let ctx = test_ctx("accelerator-cli");
        let payload = match args[1].as_str() {
            "accelerator-attach" => {
                json!({"action": "accelerator-attach", "session_id": args.get(2), "details": parse_json_arg(args.get(3))})
            }
            _ => unreachable!(),
        };
        assert_eq!(payload["action"], "accelerator-attach");
        assert_eq!(payload["session_id"], "sess-ml");
        assert_eq!(payload["details"]["cuda_lane"], "cu124");
        assert_eq!(payload["details"]["require_gpu"], true);
        let _ = fs::remove_dir_all(ctx.root);
    }

    #[test]
    fn execution_contract_records_truth_ownership() {
        let ctx = test_ctx("execution-contract");
        let req = json!({
            "session_id": "sess-ml",
            "details": {
                "command": ["python", "train.py"],
                "cuda_lane": "cu121",
                "confidence_threshold": 0.98
            }
        });
        let contract = build_execution_contract(&ctx, "sess-ml", &req).unwrap();
        assert_eq!(contract["schema"], "gema.execution_contract.v1");
        assert_eq!(contract["mode"], "twin_compute_with_cuda_api_remoting");
        assert_eq!(contract["command"]["argv"][0], "python");
        assert_eq!(contract["image"]["cuda_lane"], "cu121");
        assert_eq!(
            contract["cuda_proxy"]["primary_data_plane"],
            "gvphase1_binary_cuda_api_remoting"
        );
        assert_eq!(
            contract["state_ownership"]["workspace_files"],
            "workspace_volume"
        );
        assert_eq!(contract["governance"]["confidence_gate"], 0.98);
        assert_eq!(
            contract["worker_protocol"]["fabric_bridge_role"],
            "control_plane"
        );
        assert!(contract["checksum"]
            .as_str()
            .unwrap_or("")
            .starts_with("sha256:"));
        let _ = fs::remove_dir_all(ctx.root);
    }

    #[test]
    fn fabric_manifest_records_bridge_truth() {
        let ctx = test_ctx("fabric-manifest");
        let req = json!({
            "session_id": "sess-fabric",
            "details": {
                "command": ["python", "train.py"],
                "cuda_lane": "cu124",
                "nodes": [
                    {"id": "node-0", "role": "head", "provider": "vast", "gpu": {"name": "RTX 4090"}, "cuda_lane": "cu124", "transport": {"kind": "ssh"}, "status": "ready"},
                    {"id": "node-1", "role": "worker", "provider": "vast", "gpu": {"name": "L40"}, "cuda_lane": "cu124", "transport": {"kind": "ssh"}, "status": "ready"}
                ],
                "bridge": {"required": true, "status": "ready", "transport": "gema-ssh-stdio-bridge"},
                "raw_tcp_mesh": {"required": false, "ok": false, "required_port": 29500}
            }
        });
        let manifest = build_fabric_manifest(&ctx, "sess-fabric", &req).unwrap();
        assert_eq!(manifest["schema"], "gema.fabric_manifest.v1");
        assert_eq!(manifest["fabric"]["transport"], "gema-ssh-stdio-bridge");
        assert_eq!(manifest["fabric"]["classification"], "control_plane");
        assert_eq!(manifest["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(
            manifest["data_plane"]["primary"]["kind"],
            "gvphase1_binary_cuda_api_remoting"
        );
        assert_eq!(manifest["data_plane"]["control_bridge"]["status"], "ready");
        assert_eq!(manifest["data_plane"]["raw_tcp_mesh"]["ok"], false);
        assert!(manifest["checksum"]
            .as_str()
            .unwrap_or("")
            .starts_with("sha256:"));
        let _ = fs::remove_dir_all(ctx.root);
    }

    #[test]
    fn runtime_overlay_preflight_rejects_rootfs_outside_managed_tree() {
        let ctx = test_ctx("runtime-overlay-outside");
        let outside = ctx.root.join("outside-rootfs");
        fs::create_dir_all(outside.join("usr/bin")).unwrap();
        fs::create_dir_all(outside.join("bin")).unwrap();
        fs::write(outside.join(".gema-rootfs-ready"), b"ready").unwrap();
        fs::write(outside.join("usr/bin/env"), b"").unwrap();
        fs::write(outside.join("usr/bin/setpriv"), b"").unwrap();
        fs::write(outside.join("bin/bash"), b"").unwrap();

        let err = runtime_overlay_preflight(&ctx, "sess-overlay", &outside).unwrap_err();
        assert!(err.contains("managed rootfs directory"));
        let _ = fs::remove_dir_all(ctx.root);
    }

    #[test]
    fn runtime_overlay_preflight_records_per_session_overlay_contract() {
        if !overlay_filesystem_available() {
            return;
        }
        let ctx = test_ctx("runtime-overlay-contract");
        let base = ctx.root.join("rootfs").join("noble-base");
        fs::create_dir_all(base.join("usr/bin")).unwrap();
        fs::create_dir_all(base.join("bin")).unwrap();
        fs::write(base.join(".gema-rootfs-ready"), b"ready").unwrap();
        fs::write(base.join("usr/bin/env"), b"").unwrap();
        fs::write(base.join("usr/bin/setpriv"), b"").unwrap();
        fs::write(base.join("bin/bash"), b"").unwrap();

        let preflight = runtime_overlay_preflight(&ctx, "sess-overlay", &base).unwrap();
        assert_eq!(preflight["mode"], "per-session-overlay");
        assert_eq!(preflight["shared_rootfs_direct_mode"], "disabled");
        assert_ne!(preflight["runtime_rootfs"], preflight["base_rootfs"]);
        let _ = fs::remove_dir_all(ctx.root);
    }

    fn test_ctx(name: &str) -> Ctx {
        let root = env::temp_dir().join(format!(
            "gema-hostd-{name}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let run_dir = root.join("run");
        let log_dir = root.join("logs");
        let ports_dir = root.join("ports");
        let images_dir = root.join("images");
        Ctx {
            socket_path: run_dir.join("hostd.sock"),
            pid_file: run_dir.join("hostd.pid"),
            substrate: PathBuf::from("/bin/true"),
            port_allocations_path: ports_dir.join("allocations.json"),
            port_audit_path: ports_dir.join("audit.jsonl"),
            image_catalog_path: images_dir.join("catalog.json"),
            host_probe_path: root.join("host_probe.latest.json"),
            metrics_interval: Duration::from_secs(10),
            port_pool_start: 20000,
            port_pool_end: 29999,
            default_storage_quota_bytes: 1024 * 1024,
            snapshot_retention_count: 2,
            max_log_bytes: 1024 * 1024,
            log_retention_count: 2,
            root,
            run_dir,
            log_dir,
            ports_dir,
            images_dir: images_dir.clone(),
            image_cache_dir: images_dir.join("cache"),
            volumes_dir: images_dir.join("volumes"),
        }
    }
}
