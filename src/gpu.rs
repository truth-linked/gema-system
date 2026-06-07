use crate::capabilities::{command_exists, command_output, escape};

#[derive(Debug)]
pub struct GpuReport {
    pub nvidia_present: bool,
    pub nvidia_tools_present: bool,
    pub nvidia_driver: Option<String>,
    pub cuda_visible: Option<String>,
    pub nvenc_probe: String,
    pub amd_present: bool,
    pub amd_tools_present: bool,
    pub rocm_probe: String,
}

pub fn probe_gpus() -> GpuReport {
    let pci = command_output("lspci", &[])
        .unwrap_or_default()
        .to_ascii_lowercase();
    let nvidia_present = pci
        .lines()
        .any(|line| gpu_line(line) && line.contains("nvidia"));
    let amd_present = pci.lines().any(|line| {
        gpu_line(line)
            && (line.contains("advanced micro devices")
                || line.contains("amd/ati")
                || line.contains("ati technologies")
                || line.contains(" radeon ")
                || line.contains(" instinct ")
                || line.contains(" mi"))
    });
    let nvidia_tools_present = command_exists("nvidia-smi");
    let amd_tools_present = command_exists("rocm-smi");
    let nvidia_driver = if nvidia_tools_present {
        command_output(
            "nvidia-smi",
            &["--query-gpu=driver_version", "--format=csv,noheader"],
        )
        .map(|value| value.lines().next().unwrap_or("").trim().to_string())
    } else {
        None
    };
    let cuda_visible = if nvidia_tools_present {
        command_output(
            "nvidia-smi",
            &["--query-gpu=cuda_version", "--format=csv,noheader"],
        )
        .or_else(|| command_output("nvidia-smi", &["--query"]).map(|_| "unknown".to_string()))
    } else {
        None
    };

    GpuReport {
        nvidia_present,
        nvidia_tools_present,
        nvidia_driver,
        cuda_visible,
        nvenc_probe: if nvidia_present && nvidia_tools_present {
            "pending-nvenc-runtime-check".to_string()
        } else if nvidia_present {
            "nvidia-tools-missing".to_string()
        } else {
            "nvidia-hardware-missing".to_string()
        },
        amd_present,
        amd_tools_present,
        rocm_probe: if amd_present && amd_tools_present {
            "pending-rocm-runtime-check".to_string()
        } else if amd_present {
            "rocm-tools-missing".to_string()
        } else {
            "amd-hardware-missing".to_string()
        },
    }
}

impl GpuReport {
    pub fn to_json(&self) -> String {
        format!(
            concat!(
                "{{",
                "\"nvidia_present\":{},",
                "\"nvidia_tools_present\":{},",
                "\"nvidia_driver\":{},",
                "\"cuda_visible\":{},",
                "\"nvenc_probe\":\"{}\",",
                "\"amd_present\":{},",
                "\"amd_tools_present\":{},",
                "\"rocm_probe\":\"{}\"",
                "}}"
            ),
            self.nvidia_present,
            self.nvidia_tools_present,
            opt_json(&self.nvidia_driver),
            opt_json(&self.cuda_visible),
            escape(&self.nvenc_probe),
            self.amd_present,
            self.amd_tools_present,
            escape(&self.rocm_probe)
        )
    }
}

fn gpu_line(line: &str) -> bool {
    line.contains("vga") || line.contains("3d controller") || line.contains("display")
}

fn opt_json(value: &Option<String>) -> String {
    value
        .as_ref()
        .map(|inner| format!("\"{}\"", escape(inner)))
        .unwrap_or_else(|| "null".to_string())
}
