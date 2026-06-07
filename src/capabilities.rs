use std::path::Path;
use std::process::Command;

#[derive(Debug)]
pub struct HostCapabilityReport {
    pub kernel: String,
    pub cgroup_mode: String,
    pub runc: bool,
    pub crun: bool,
    pub buildah: bool,
    pub btrfs: bool,
    pub zfs: bool,
    pub nvidia_smi: bool,
    pub nvidia_ctk: bool,
    pub rocm_smi: bool,
}

pub fn probe_host() -> HostCapabilityReport {
    HostCapabilityReport {
        kernel: command_output("uname", &["-r"]).unwrap_or_else(|| "unknown".to_string()),
        cgroup_mode: cgroup_mode(),
        runc: command_exists("runc"),
        crun: command_exists("crun"),
        buildah: command_exists("buildah"),
        btrfs: command_exists("btrfs"),
        zfs: command_exists("zfs"),
        nvidia_smi: command_exists("nvidia-smi"),
        nvidia_ctk: command_exists("nvidia-ctk"),
        rocm_smi: command_exists("rocm-smi"),
    }
}

impl HostCapabilityReport {
    pub fn to_json(&self) -> String {
        format!(
            concat!(
                "{{",
                "\"kernel\":\"{}\",",
                "\"cgroup_mode\":\"{}\",",
                "\"runc\":{},",
                "\"crun\":{},",
                "\"buildah\":{},",
                "\"btrfs\":{},",
                "\"zfs\":{},",
                "\"nvidia_smi\":{},",
                "\"nvidia_ctk\":{},",
                "\"rocm_smi\":{}",
                "}}"
            ),
            escape(&self.kernel),
            escape(&self.cgroup_mode),
            self.runc,
            self.crun,
            self.buildah,
            self.btrfs,
            self.zfs,
            self.nvidia_smi,
            self.nvidia_ctk,
            self.rocm_smi
        )
    }
}

fn cgroup_mode() -> String {
    if Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        "v2".to_string()
    } else if Path::new("/sys/fs/cgroup/memory").exists() {
        "v1".to_string()
    } else {
        "unknown".to_string()
    }
}

pub fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .args([
            "-lc",
            &format!("command -v {} >/dev/null 2>&1", shell_quote(name)),
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn command_output(name: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(name).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
