//! `nds-forward-pool`: one loopback listener in front of redundant forwards to one service.

use nddev_device_sync_cache_pool::{Pool, Probe, Upstream, serve, watch};
use std::{net::SocketAddr, process::ExitCode, sync::Arc};

const USAGE: &str = "usage:
  nds-forward-pool --listen <loopback address:port>
    (--primary <loopback address:port>)... [--fallback <loopback address:port>]...
    (--http-probe <path> | --banner-probe <prefix>) [--interval <seconds, 1 to 300>]";

#[tokio::main]
async fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let mut listen: Option<SocketAddr> = None;
    let mut upstreams = Vec::new();
    let mut probe = None;
    let mut interval = std::time::Duration::from_secs(2);
    let mut words = arguments.iter();
    while let Some(word) = words.next() {
        let Some(value) = words.next() else {
            return usage();
        };
        let loopback = || {
            value
                .parse::<SocketAddr>()
                .ok()
                .filter(|address| address.ip().is_loopback())
        };
        match word.as_str() {
            "--listen" => listen = loopback(),
            "--primary" | "--fallback" => match loopback() {
                Some(address) => upstreams.push(Upstream::new(address, word == "--fallback")),
                None => return usage(),
            },
            "--http-probe" if value.starts_with('/') && !value.contains(['\r', '\n', ' ']) => {
                probe = Some(Probe::Http(value.clone()));
            }
            "--banner-probe" if !value.is_empty() => probe = Some(Probe::Banner(value.clone())),
            "--interval" => match value.parse::<u64>() {
                Ok(seconds @ 1..=300) => interval = std::time::Duration::from_secs(seconds),
                _ => return usage(),
            },
            _ => return usage(),
        }
    }
    let (Some(listen), Some(probe)) = (listen, probe) else {
        return usage();
    };
    if !upstreams.iter().any(|upstream| !upstream.fallback) {
        return usage();
    }
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("event=pool.listen outcome=failed error={}", error.kind());
            return ExitCode::FAILURE;
        }
    };
    let mut pool = Pool::new(upstreams, probe);
    pool.interval = interval;
    let pool = Arc::new(pool);
    // Connections wait for the first round of probes, so the first ones never go to a
    // forward that is still down after a restart.
    pool.check().await;
    eprintln!("event=pool.listen outcome=ready");
    tokio::spawn(watch(pool.clone()));
    tokio::select! {
        result = serve(listener, pool) => match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("event=pool.accept outcome=failed error={}", error.kind());
                ExitCode::FAILURE
            }
        },
        _ = shutdown() => ExitCode::SUCCESS,
    }
}

async fn shutdown() {
    if let Ok(mut terminate) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        return;
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}
