//! The relay against real loopback sockets.

use nddev_device_sync_cache_egress::{Limits, Policy, serve};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// A destination that echoes every byte back.
async fn echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut read, mut write) = stream.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    address
}

async fn relay(policy: Policy, limits: Limits) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, policy, limits));
    address
}

/// Sends a request head and returns the status line of the answer, with the open stream.
async fn request(relay: SocketAddr, head: &str) -> (String, TcpStream) {
    let mut stream = TcpStream::connect(relay).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    let mut byte = [0u8; 1];
    while !answer.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await.unwrap() == 0 {
            break;
        }
        answer.push(byte[0]);
    }
    let text = String::from_utf8(answer).unwrap();
    (text.lines().next().unwrap_or_default().to_owned(), stream)
}

fn fixture(port: u16) -> Policy {
    Policy {
        ports: vec![port],
        loopback: true,
    }
}

#[tokio::test]
async fn a_tunnel_carries_bytes_both_ways_end_to_end() {
    let target = echo().await;
    let relay = relay(fixture(target.port()), Limits::default()).await;
    let (status, mut stream) = request(
        relay,
        &format!(
            "CONNECT localhost:{} HTTP/1.1\r\nHost: x\r\n\r\n",
            target.port()
        ),
    )
    .await;
    assert_eq!(status, "HTTP/1.1 200 Connection Established");
    stream.write_all(b"tls bytes stay opaque").await.unwrap();
    let mut echoed = [0u8; 21];
    stream.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"tls bytes stay opaque");
}

#[tokio::test]
async fn private_destinations_ports_and_other_methods_are_refused() {
    let target = echo().await;
    // The shipped policy never opens loopback, even on an allowed port.
    let strict = relay(
        Policy {
            ports: vec![target.port()],
            loopback: false,
        },
        Limits::default(),
    )
    .await;
    for host in [
        "127.0.0.1",
        "localhost",
        "[::1]",
        "10.0.0.1",
        "169.254.169.254",
    ] {
        let (status, _) = request(
            strict,
            &format!("CONNECT {host}:{} HTTP/1.1\r\n\r\n", target.port()),
        )
        .await;
        assert_eq!(status, "HTTP/1.1 403 Forbidden", "{host}");
    }
    let open = relay(fixture(target.port()), Limits::default()).await;
    for (head, expected) in [
        (
            "CONNECT localhost:22 HTTP/1.1\r\n\r\n".to_owned(),
            "HTTP/1.1 403 Forbidden",
        ),
        (
            "GET http://localhost/ HTTP/1.1\r\n\r\n".to_owned(),
            "HTTP/1.1 405 Method Not Allowed",
        ),
        (
            "CONNECT localhost HTTP/1.1\r\n\r\n".to_owned(),
            "HTTP/1.1 400 Bad Request",
        ),
        (
            format!(
                "CONNECT localhost:443 HTTP/1.1\r\nX: {}\r\n\r\n",
                "a".repeat(9000)
            ),
            "HTTP/1.1 431 Request Header Fields Too Large",
        ),
    ] {
        let (status, _) = request(open, &head).await;
        assert_eq!(status, expected, "{}", &head[..head.len().min(40)]);
    }
}

#[tokio::test]
async fn a_slow_head_times_out_and_capacity_is_bounded() {
    let target = echo().await;
    let limits = Limits {
        head_timeout: Duration::from_millis(200),
        connections: 1,
        ..Limits::default()
    };
    let relay = relay(fixture(target.port()), limits).await;
    let mut silent = TcpStream::connect(relay).await.unwrap();
    let mut answer = String::new();
    silent.read_to_string(&mut answer).await.unwrap();
    assert!(
        answer.starts_with("HTTP/1.1 408 Request Timeout"),
        "{answer}"
    );

    let (status, _open) = request(
        relay,
        &format!("CONNECT localhost:{} HTTP/1.1\r\n\r\n", target.port()),
    )
    .await;
    assert_eq!(status, "HTTP/1.1 200 Connection Established");
    let (status, _) = request(
        relay,
        &format!("CONNECT localhost:{} HTTP/1.1\r\n\r\n", target.port()),
    )
    .await;
    assert_eq!(status, "HTTP/1.1 503 Service Unavailable");
}
