use crate::capabilities::{command_exists, escape};
use crate::gpu;

#[derive(Debug)]
pub struct SupervisorStatus {
    pub state: String,
    pub oci_backend: String,
    pub filesystem_snapshot_backend: String,
    pub gpu_backend: String,
    pub warning: String,
}

pub fn status_report() -> SupervisorStatus {
    let oci_backend = if command_exists("runc") {
        "runc"
    } else if command_exists("crun") {
        "crun"
    } else {
        "missing"
    };
    let filesystem_snapshot_backend = if command_exists("btrfs") {
        "btrfs"
    } else if command_exists("zfs") {
        "zfs"
    } else {
        "overlay-delta"
    };
    let gpu_report = gpu::probe_gpus();
    let gpu_backend = if gpu_report.nvidia_present {
        "nvidia"
    } else if gpu_report.amd_present {
        "amd-rocm"
    } else {
        "cpu-only"
    };

    SupervisorStatus {
        state: "design-seeded".to_string(),
        oci_backend: oci_backend.to_string(),
        filesystem_snapshot_backend: filesystem_snapshot_backend.to_string(),
        gpu_backend: gpu_backend.to_string(),
        warning: "native launch is handled by the Rust host daemon through the hostd socket"
            .to_string(),
    }
}

impl SupervisorStatus {
    pub fn to_json(&self) -> String {
        format!(
            concat!(
                "{{",
                "\"state\":\"{}\",",
                "\"oci_backend\":\"{}\",",
                "\"filesystem_snapshot_backend\":\"{}\",",
                "\"gpu_backend\":\"{}\",",
                "\"warning\":\"{}\"",
                "}}"
            ),
            escape(&self.state),
            escape(&self.oci_backend),
            escape(&self.filesystem_snapshot_backend),
            escape(&self.gpu_backend),
            escape(&self.warning)
        )
    }
}
