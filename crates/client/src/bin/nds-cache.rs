//! `nds-cache`: binds the current user's tools, or one CI lane's, to the NDS network cache.
//! `--lane` reads the environment that the lane's jobs run with.

use std::process::ExitCode;

const USAGE: &str = "usage:
  nds-cache (status | bind | unbind) --port <loopback port> [--lane <lane root>]\n  nds-cache buildkit-config --port <loopback port>\n  nds-cache pub-get --port <loopback port> --project <directory> --pub-cache <fresh absolute path> --dart <absolute executable>";

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() -> ExitCode {
    eprintln!("nds-cache is not supported on this platform");
    ExitCode::from(2)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    use nddev_device_sync_cache_client::{
        CancellationToken, Client, NativeIo, NetworkCache, Scope, binding::Base,
    };
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = arguments.iter().map(String::as_str).collect();
    if let [
        "pub-get",
        "--port",
        port,
        "--project",
        project,
        "--pub-cache",
        cache,
        "--dart",
        dart,
    ] = words.as_slice()
    {
        let Some(base) = port.parse().ok().and_then(Base::new) else {
            return usage();
        };
        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        let mut termination =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(_) => return ExitCode::from(2),
            };
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => signal.cancel(),
                _ = termination.recv() => signal.cancel(),
            }
        });
        let options = nddev_device_sync_cache_client::pub_get::Options {
            base,
            project: (*project).into(),
            cache: (*cache).into(),
            dart: (*dart).into(),
        };
        return match nddev_device_sync_cache_client::pub_get::run(&options, cancellation).await {
            Ok(report) => {
                println!("{}", serde_json::to_string(&report).unwrap_or_default());
                ExitCode::SUCCESS
            }
            Err(error) => {
                println!(
                    "{}",
                    serde_json::json!({"error": error.code(), "message": error.to_string()})
                );
                ExitCode::from(2)
            }
        };
    }
    let (command, port, root) = match words.as_slice() {
        [command, "--port", port] => (*command, *port, None),
        [command, "--port", port, "--lane", root] => (*command, *port, Some(*root)),
        _ => return usage(),
    };
    let Some(base) = port.parse().ok().and_then(Base::new) else {
        return usage();
    };
    if command == "buildkit-config" {
        if root.is_some() {
            return usage();
        }
        print!(
            "{}",
            nddev_device_sync_cache_client::oci::buildkit_config(&base)
        );
        return ExitCode::SUCCESS;
    }
    let scope = match root {
        Some(root) => Scope::lane(std::path::Path::new(root)),
        None => Scope::user(),
    };
    let client = match (scope, NativeIo::new(1)) {
        (Ok(scope), Ok(io)) => Client::new(io.local_handle(), scope, base),
        (Err(error), _) => return failed(&error),
        (_, Err(error)) => return failed(&error.into()),
    };
    let cancellation = CancellationToken::new();
    let result = match command {
        "status" => client.status(cancellation).await,
        "bind" => client.bind(cancellation).await,
        "unbind" => client.unbind(cancellation).await,
        _ => return usage(),
    };
    match result {
        Ok(status) => {
            println!("{}", serde_json::to_string(&status).unwrap_or_default());
            ExitCode::SUCCESS
        }
        Err(error) => failed(&error),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn failed(error: &nddev_device_sync_cache_client::Error) -> ExitCode {
    println!(
        "{}",
        serde_json::json!({ "error": error.code(), "message": error.to_string() })
    );
    ExitCode::from(2)
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}
