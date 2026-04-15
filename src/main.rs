use std::ffi::OsString;
use std::net::{SocketAddr, ToSocketAddrs};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use env_logger::Env;
use log::LevelFilter;
use phantom_rs::{ProxyPrefs, ProxyServer, ServerProtocol};
use phantom_rs::relay::{run_relay_host, run_relay_player, RelayHostConfig, RelayPlayerConfig};

#[derive(Parser, Debug)]
#[command(name = "phantom")]
#[command(about = "Makes hosted Bedrock/MCPE servers show up as LAN servers")]
struct Cli {
    #[arg(long = "server")]
    server: Option<String>,

    #[arg(index = 1, hide = true)]
    server_positional: Option<String>,

    #[arg(long = "bind", default_value = "0.0.0.0")]
    bind: String,

    #[arg(long = "bind_port", default_value_t = 0)]
    bind_port: u16,

    #[arg(long = "relay")]
    relay: Option<String>,

    #[arg(long = "room", default_value = "bedrock1")]
    room: String,

    #[arg(long = "relay_role", value_enum)]
    relay_role: Option<CliRelayRole>,

    #[arg(long = "direct_relay", default_value_t = false)]
    direct_relay: bool,

    #[arg(long = "timeout", default_value_t = 60)]
    timeout: u64,

    #[arg(long = "debug", default_value_t = false)]
    debug: bool,

    #[arg(short = '6', long = "ipv6", default_value_t = false)]
    ipv6: bool,

    #[arg(long = "remove_ports", default_value_t = false)]
    remove_ports: bool,

    #[arg(long = "workers", default_value_t = 1)]
    workers: usize,

    #[arg(long = "server_protocol", value_enum, default_value_t = CliServerProtocol::Raknet)]
    server_protocol: CliServerProtocol,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum CliServerProtocol {
    Raknet,
    #[value(alias = "nerthernet")]
    Nethernet,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum CliRelayRole {
    Host,
    Player,
}

impl From<CliServerProtocol> for ServerProtocol {
    fn from(value: CliServerProtocol) -> Self {
        match value {
            CliServerProtocol::Raknet => ServerProtocol::Raknet,
            CliServerProtocol::Nethernet => ServerProtocol::Nethernet,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse_from(normalize_legacy_args(std::env::args_os()));

    let log_level = if cli.debug {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };

    env_logger::Builder::from_env(Env::default().default_filter_or(log_level.as_str())).init();

    let relay_role = if cli.direct_relay {
        Some(CliRelayRole::Host)
    } else {
        cli.relay_role
    };

    if let Some(role) = relay_role {
        let relay_url = match cli.relay {
            Some(v) => v,
            None => {
                eprintln!("Did you forget --relay?");
                process::exit(1);
            }
        };

        match role {
            CliRelayRole::Host => {
                let server = match cli.server.or(cli.server_positional) {
                    Some(value) => value,
                    None => {
                        eprintln!("Did you forget --server?");
                        process::exit(1);
                    }
                };

                let protocol = cli.server_protocol.into();
                let target_server = resolve_socket_addr(&server, protocol).map_err(|e| {
                    format!("invalid --server value. expected host:port or host (for nethernet), got {server}: {e}")
                })?;

                let rt = tokio::runtime::Runtime::new()?;
                return rt.block_on(run_relay_host(RelayHostConfig {
                    relay_url,
                    room: cli.room,
                    target_server,
                    server_protocol: protocol,
                }));
            }
            CliRelayRole::Player => {
                let bind_port = if cli.bind_port == 0 { 19132 } else { cli.bind_port };
                let rt = tokio::runtime::Runtime::new()?;
                return rt.block_on(run_relay_player(RelayPlayerConfig {
                    relay_url,
                    room: cli.room,
                    bind_port,
                }));
            }
        }
    }

    let server = match cli.server.or(cli.server_positional) {
        Some(value) => value,
        None => {
            eprintln!("Did you forget --server?");
            process::exit(1);
        }
    };

    println!("Starting up with remote server IP: {}", server);

    let proxy = Arc::new(ProxyServer::new(ProxyPrefs {
        bind_address: cli.bind,
        bind_port: cli.bind_port,
        remote_server: server,
        server_protocol: cli.server_protocol.into(),
        idle_timeout: Duration::from_secs(cli.timeout),
        enable_ipv6: cli.ipv6,
        remove_ports: cli.remove_ports,
        num_workers: cli.workers,
    })?);

    let force_quit = Arc::new(AtomicBool::new(false));
    let quit_proxy = Arc::clone(&proxy);
    let quit_flag = Arc::clone(&force_quit);

    ctrlc::set_handler(move || {
        if quit_flag.swap(true, Ordering::SeqCst) {
            println!("\nForce quitting");
            process::exit(2);
        }

        println!("\nPress CTRL + C again to force quit");
        quit_proxy.close();
    })?;

    proxy.start()?;

    Ok(())
}

fn resolve_socket_addr(input: &str, protocol: ServerProtocol) -> Result<SocketAddr, String> {
    if input.contains(':') {
        return input
            .to_socket_addrs()
            .map_err(|e| e.to_string())?
            .next()
            .ok_or_else(|| "could not resolve address".to_string());
    }
    if protocol == ServerProtocol::Nethernet {
        let addr = format!("{}:19132", input);
        return addr
            .to_socket_addrs()
            .map_err(|e| e.to_string())?
            .next()
            .ok_or_else(|| "could not resolve address".to_string());
    }
    Err("expected host:port".to_string())
}

// Keep compatibility with original phantom flag style (e.g. `-server`),
// while still using clap's standard long flag parsing.
fn normalize_legacy_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    args.into_iter()
        .map(|arg| match arg.to_str() {
            Some("-server") => OsString::from("--server"),
            Some("-bind") => OsString::from("--bind"),
            Some("-bind_port") => OsString::from("--bind_port"),
            Some("-timeout") => OsString::from("--timeout"),
            Some("-debug") => OsString::from("--debug"),
            Some("-remove_ports") => OsString::from("--remove_ports"),
            Some("-workers") => OsString::from("--workers"),
            Some("-server_protocol") => OsString::from("--server_protocol"),
            Some("-relay") => OsString::from("--relay"),
            Some("-room") => OsString::from("--room"),
            Some("-relay_role") => OsString::from("--relay_role"),
            Some("-direct_relay") => OsString::from("--direct_relay"),
            _ => arg,
        })
        .collect()
}
