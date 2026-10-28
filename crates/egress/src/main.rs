//! `nds-cache-egress`: the loopback HTTPS egress relay of the cache host.

use nddev_device_sync_cache::telemetry;
use nddev_device_sync_cache_egress::{Limits, Policy, serve};
use std::{net::SocketAddr, process::ExitCode};

const USAGE: &str = "usage:
  nds-cache-egress --listen <loopback address:port> [--allow-port <port>]... [--max-connections <n>]";

#[tokio::main(worker_threads = 4)]
async fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let mut listen: Option<SocketAddr> = None;
    let mut ports = Vec::new();
    let mut limits = Limits::default();
    let mut words = arguments.iter();
    while let Some(word) = words.next() {
        let value = words.next();
        match (word.as_str(), value) {
            ("--listen", Some(value)) => listen = value.parse().ok(),
            ("--allow-port", Some(value)) => match value.parse::<u16>() {
                Ok(port) if port > 0 => ports.push(port),
                _ => return usage(),
            },
            ("--max-connections", Some(value)) => match value.parse::<usize>() {
                Ok(count) if count > 0 => limits.connections = count,
                _ => return usage(),
            },
            _ => return usage(),
        }
    }
    // The relay answers only its own host's loopback; devices reach it through their forward.
    let Some(listen) = listen.filter(|address| address.ip().is_loopback()) else {
        return usage();
    };
    let mut policy = Policy::default();
    if !ports.is_empty() {
        policy.ports = ports;
    }
    let get = |name: &str| std::env::var(name).ok();
    let Ok(identity) = telemetry::Identity::read(&get) else {
        eprintln!("invalid_telemetry_configuration");
        return ExitCode::FAILURE;
    };
    let _logger = match telemetry::install(&identity, &get) {
        Ok(logger) => logger,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(_) => {
            tracing::error!(module = "cache", event.name = "egress.listen.failed", outcome = "error", error.type = "io");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(
        module = "cache",
        event.name = "egress.listening",
        outcome = "ok"
    );
    match serve(listener, policy, limits, shutdown()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => {
            tracing::error!(module = "cache", event.name = "egress.accept.failed", outcome = "error", error.type = "io");
            ExitCode::FAILURE
        }
    }
}

async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}
