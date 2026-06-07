use crate::capabilities::escape;

#[derive(Debug, Clone)]
pub struct GemaWorkloadSpec {
    pub user_id: String,
    pub session_id: String,
    pub image: String,
    pub profile: String,
    pub cpu_cores: f64,
    pub ram_mb: i64,
    pub disk_gb: i64,
    pub gpu_vendor: String,
    pub checkpoint_interval_seconds: i64,
    pub workspace_mount: String,
    pub agent_mount: String,
}

impl GemaWorkloadSpec {
    pub fn example() -> Self {
        Self {
            user_id: "user-id".to_string(),
            session_id: "session-id".to_string(),
            image: "ghcr.io/gematech/gema_cloud/os-ml:latest".to_string(),
            profile: "ml".to_string(),
            cpu_cores: 4.0,
            ram_mb: 8192,
            disk_gb: 100,
            gpu_vendor: "nvidia".to_string(),
            checkpoint_interval_seconds: 120,
            workspace_mount: "/var/lib/gema/workspaces/session-id/workspace".to_string(),
            agent_mount: "/var/lib/gema/workspaces/session-id/agent".to_string(),
        }
    }

    pub fn to_json(&self) -> String {
        format!(
            concat!(
                "{{",
                "\"user_id\":\"{}\",",
                "\"session_id\":\"{}\",",
                "\"image\":\"{}\",",
                "\"profile\":\"{}\",",
                "\"cpu_cores\":{},",
                "\"ram_mb\":{},",
                "\"disk_gb\":{},",
                "\"gpu_vendor\":\"{}\",",
                "\"checkpoint_interval_seconds\":{},",
                "\"workspace_mount\":\"{}\",",
                "\"agent_mount\":\"{}\"",
                "}}"
            ),
            escape(&self.user_id),
            escape(&self.session_id),
            escape(&self.image),
            escape(&self.profile),
            self.cpu_cores,
            self.ram_mb,
            self.disk_gb,
            escape(&self.gpu_vendor),
            self.checkpoint_interval_seconds,
            escape(&self.workspace_mount),
            escape(&self.agent_mount)
        )
    }
}
