use std::ffi::OsString;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use env_logger::Env;
use log::LevelFilter;
use phantom_rs::{ProxyPrefs, ProxyServer};

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
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse_from(normalize_legacy_args(std::env::args_os()));

    let server = match cli.server.or(cli.server_positional) {
        Some(value) => value,
        None => {
            eprintln!("Did you forget --server?");
            process::exit(1);
        }
    };

    let log_level = if cli.debug {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };

    env_logger::Builder::from_env(Env::default().default_filter_or(log_level.as_str())).init();

    println!("Starting up with remote server IP: {}", server);

    let proxy = Arc::new(ProxyServer::new(ProxyPrefs {
        bind_address: cli.bind,
        bind_port: cli.bind_port,
        remote_server: server,
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
            _ => arg,
        })
        .collect()
}
