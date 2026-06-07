mod capabilities;
mod gpu;
mod hostd;
mod ports;
mod spec;
mod supervisor;

use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("probe") => {
            let report = capabilities::probe_host();
            println!("{}", report.to_json());
            ExitCode::SUCCESS
        }
        Some("gpu") => {
            let report = gpu::probe_gpus();
            println!("{}", report.to_json());
            ExitCode::SUCCESS
        }
        Some("ports") => {
            let reserved = ports::reserved_platform_ports(47990, 63090);
            println!("{}", ports::ports_to_json(&reserved));
            ExitCode::SUCCESS
        }
        Some("port-allowed") => {
            let Some(port) = args.next().and_then(|value| value.parse::<u16>().ok()) else {
                eprintln!("usage: gema-runtime port-allowed <port>");
                return ExitCode::from(64);
            };
            println!(
                "{{\"port\":{},\"allowed\":{}}}",
                port,
                ports::user_port_is_allowed(port)
            );
            ExitCode::SUCCESS
        }
        Some("spec-example") => {
            println!("{}", spec::GemaWorkloadSpec::example().to_json());
            ExitCode::SUCCESS
        }
        Some("supervisor-status") => {
            println!("{}", supervisor::status_report().to_json());
            ExitCode::SUCCESS
        }
        Some("hostd") => hostd::main(args.collect()),
        _ => {
            eprintln!("usage: gema-runtime probe|gpu|ports|port-allowed <port>|spec-example|supervisor-status|hostd ...");
            ExitCode::from(64)
        }
    }
}
