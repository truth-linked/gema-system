#[derive(Debug, Clone)]
pub struct PortBinding {
    pub name: &'static str,
    pub protocol: &'static str,
    pub internal: u16,
    pub external: u16,
}

pub fn reserved_platform_ports(transport_base: u16, ssh_port: u16) -> Vec<PortBinding> {
    vec![
        PortBinding {
            name: "sunshine-web",
            protocol: "tcp",
            internal: 47990,
            external: transport_base,
        },
        PortBinding {
            name: "moonlight-control",
            protocol: "tcp",
            internal: 47984,
            external: transport_base + 1,
        },
        PortBinding {
            name: "moonlight-rtsp",
            protocol: "tcp",
            internal: 47989,
            external: transport_base + 2,
        },
        PortBinding {
            name: "sunshine-https",
            protocol: "tcp",
            internal: 48010,
            external: transport_base + 3,
        },
        PortBinding {
            name: "moonlight-udp-1",
            protocol: "udp",
            internal: 47998,
            external: transport_base + 4,
        },
        PortBinding {
            name: "moonlight-udp-2",
            protocol: "udp",
            internal: 47999,
            external: transport_base + 5,
        },
        PortBinding {
            name: "moonlight-udp-3",
            protocol: "udp",
            internal: 48000,
            external: transport_base + 6,
        },
        PortBinding {
            name: "moonlight-udp-4",
            protocol: "udp",
            internal: 48002,
            external: transport_base + 7,
        },
        PortBinding {
            name: "moonlight-udp-5",
            protocol: "udp",
            internal: 48010,
            external: transport_base + 8,
        },
        PortBinding {
            name: "gema-managed-ssh",
            protocol: "tcp",
            internal: 22,
            external: ssh_port,
        },
    ]
}

pub fn user_port_is_allowed(port: u16) -> bool {
    !matches!(
        port,
        1..=1023
            | 2375
            | 2376
            | 3000
            | 5432
            | 6379
            | 11434
            | 47984
            | 47989
            | 47990
            | 47998
            | 47999
            | 48000
            | 48002
            | 48010
    )
}

pub fn ports_to_json(ports: &[PortBinding]) -> String {
    let entries = ports
        .iter()
        .map(|port| {
            format!(
                "{{\"name\":\"{}\",\"protocol\":\"{}\",\"internal\":{},\"external\":{}}}",
                port.name, port.protocol, port.internal, port.external
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("[{}]", entries)
}
