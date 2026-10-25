use std::process::ExitCode;

use nddev_device_sync_cache::{config::Settings, domain::Routes, telemetry};

const USAGE: &str = "Usage: nddev-device-sync-cache [serve|healthcheck]\n\
Required: NDS_CACHE_ROOT (absolute store directory)\n\
Optional: NDS_CACHE_LISTEN, NDS_CACHE_CAPACITY_BYTES, NDS_CACHE_MAX_OBJECT_BYTES,\n\
NDS_CACHE_FETCHES, NDS_CACHE_REVALIDATE_SECONDS";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["serve"] => serve(),
        ["healthcheck"] => healthcheck(),
        ["--help"] | ["help"] => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn serve() -> ExitCode {
    let _ = rustls::crypto::ring::default_provider().install_default();
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
    let settings = match Settings::from_env() {
        Ok(settings) => settings,
        Err(error) => {
            tracing::error!(module = "process", event.name = "process.failed", error.type = %error, outcome = "error");
            return ExitCode::FAILURE;
        }
    };
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
    else {
        tracing::error!(module = "process", event.name = "process.failed", error.type = "runtime_unavailable", outcome = "error");
        return ExitCode::FAILURE;
    };
    let result = runtime.block_on(async {
        let app = nddev_device_sync_cache::app(&settings, identity, Routes::production()).await?;
        nddev_device_sync_cache::serve(&settings, app, shutdown_signal()).await
    });
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(module = "process", event.name = "process.failed", error.type = error.class(), outcome = "error");
            ExitCode::FAILURE
        }
    }
}

async fn shutdown_signal() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            let _ = interrupt.await;
            return;
        };
        tokio::select! {
            _ = interrupt => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = interrupt.await;
}

/// Container health probe: succeeds only when `/v1/ready` answers 200 on the listener.
fn healthcheck() -> ExitCode {
    use std::io::{Read, Write};
    let listen = std::env::var("NDS_CACHE_LISTEN")
        .unwrap_or_else(|_| nddev_device_sync_cache::config::DEFAULT_LISTEN.into());
    let Ok(mut address) = listen.parse::<std::net::SocketAddr>() else {
        return ExitCode::FAILURE;
    };
    if address.ip().is_unspecified() {
        address.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
    }
    let timeout = std::time::Duration::from_secs(3);
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&address, timeout) else {
        return ExitCode::FAILURE;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let request = format!("GET /v1/ready HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    let mut response = [0u8; 16];
    let ready = stream.write_all(request.as_bytes()).is_ok()
        && stream
            .read(&mut response)
            .is_ok_and(|read| response[..read].starts_with(b"HTTP/1.1 200"));
    if ready {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
