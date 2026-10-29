//! The relay against real loopback sockets.

mod support;

use nddev_device_sync_cache_egress::{Limits, Policy};
use std::{net::SocketAddr, time::Duration};
use support::{echo, relay};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// Sends a request head and returns the status line of the answer, with the open stream.
async fn request(relay: SocketAddr, head: &str) -> (String, TcpStream) {
    tokio::time::timeout(Duration::from_secs(3), request_inner(relay, head))
        .await
        .expect("fixture response deadline")
}

async fn request_inner(relay: SocketAddr, head: &str) -> (String, TcpStream) {
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
    let target = echo();
    let relay = relay(fixture(target.address.port()), Limits::default());
    let (status, mut stream) = request(
        relay.address,
        &format!(
            "CONNECT localhost:{} HTTP/1.1\r\nHost: x\r\n\r\n",
            target.address.port()
        ),
    )
    .await;
    assert_eq!(status, "HTTP/1.1 200 Connection Established");
    stream.write_all(b"tls bytes stay opaque").await.unwrap();
    let mut echoed = [0u8; 21];
    tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut echoed))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&echoed, b"tls bytes stay opaque");
}

#[tokio::test]
async fn local_readiness_works_through_the_real_forward_pool_without_a_destination() {
    use nddev_device_sync_cache_pool::{Pool, Probe, Upstream, serve};
    use std::sync::Arc;
    let relay = relay(
        Policy {
            ports: vec![],
            loopback: false,
        },
        Limits::default(),
    );
    let pool = Arc::new(Pool::new(
        vec![Upstream::new(relay.address, false)],
        Probe::Http("/v1/ready".into()),
    ));
    pool.check().await;
    assert!(pool.upstreams[0].healthy());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(serve(listener, pool));
    let (status, mut stream) = request(
        address,
        "GET /v1/ready HTTP/1.1\r\nHost: private-fixture.invalid\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    let mut body = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut body))
        .await
        .unwrap()
        .unwrap();
    assert!(body.is_empty());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn readiness_is_an_exact_local_bodyless_get() {
    let relay = relay(Policy::default(), Limits::default());
    for head in [
        "GET /v1/ready/ HTTP/1.1\r\n\r\n",
        "GET /v1/ready?secret=value HTTP/1.1\r\n\r\n",
        "GET http://example.invalid/v1/ready HTTP/1.1\r\n\r\n",
        "HEAD /v1/ready HTTP/1.1\r\n\r\n",
        "GET /v1/ready HTTP/1.1\r\nContent-Length: 1\r\n\r\n",
        "GET /v1/ready HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
        "GET /v1/ready HTTP/1.1\r\nInvalid header\r\n\r\n",
    ] {
        assert_eq!(
            request(relay.address, head).await.0,
            "HTTP/1.1 405 Method Not Allowed"
        );
    }
    assert_eq!(
        request(
            relay.address,
            "GET /v1/ready HTTP/1.0\r\nContent-Length: 0\r\n\r\n"
        )
        .await
        .0,
        "HTTP/1.1 200 OK"
    );
}

#[tokio::test]
async fn private_destinations_ports_and_other_methods_are_refused() {
    let target = echo();
    // The shipped policy never opens loopback, even on an allowed port.
    let strict = relay(
        Policy {
            ports: vec![target.address.port()],
            loopback: false,
        },
        Limits::default(),
    );
    for host in [
        "127.0.0.1",
        "localhost",
        "[::1]",
        "10.0.0.1",
        "169.254.169.254",
    ] {
        let (status, _) = request(
            strict.address,
            &format!("CONNECT {host}:{} HTTP/1.1\r\n\r\n", target.address.port()),
        )
        .await;
        assert_eq!(status, "HTTP/1.1 403 Forbidden", "{host}");
    }
    let open = relay(fixture(target.address.port()), Limits::default());
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
        let (status, _) = request(open.address, &head).await;
        assert_eq!(status, expected, "{}", &head[..head.len().min(40)]);
    }
}

#[tokio::test]
async fn a_slow_head_times_out_and_capacity_is_bounded() {
    let target = echo();
    let limits = Limits {
        head_timeout: Duration::from_millis(200),
        connections: 1,
        ..Limits::default()
    };
    let relay = relay(fixture(target.address.port()), limits);
    let mut silent = TcpStream::connect(relay.address).await.unwrap();
    let mut answer = String::new();
    tokio::time::timeout(Duration::from_secs(3), silent.read_to_string(&mut answer))
        .await
        .unwrap()
        .unwrap();
    assert!(
        answer.starts_with("HTTP/1.1 408 Request Timeout"),
        "{answer}"
    );

    let (status, _open) = request(
        relay.address,
        &format!(
            "CONNECT localhost:{} HTTP/1.1\r\n\r\n",
            target.address.port()
        ),
    )
    .await;
    assert_eq!(status, "HTTP/1.1 200 Connection Established");
    let (status, _) = request(
        relay.address,
        &format!(
            "CONNECT localhost:{} HTTP/1.1\r\n\r\n",
            target.address.port()
        ),
    )
    .await;
    assert_eq!(status, "HTTP/1.1 503 Service Unavailable");
    assert_eq!(
        request(relay.address, "GET /v1/ready HTTP/1.1\r\n\r\n")
            .await
            .0,
        "HTTP/1.1 503 Service Unavailable"
    );
}

#[tokio::test]
async fn shutdown_joins_tunnels_before_returning_while_the_runtime_stays_alive() {
    let target = echo();
    let mut relay = relay(
        fixture(target.address.port()),
        Limits {
            connections: 1,
            ..Limits::default()
        },
    );
    let (status, mut stream) = request(
        relay.address,
        &format!(
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n",
            target.address.port()
        ),
    )
    .await;
    assert_eq!(status, "HTTP/1.1 200 Connection Established");
    // Refuse repeatedly while one tunnel is held. No refusal owns a detached task.
    for _ in 0..16 {
        assert_eq!(
            request(
                relay.address,
                &format!(
                    "CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n",
                    target.address.port()
                )
            )
            .await
            .0,
            "HTTP/1.1 503 Service Unavailable"
        );
    }
    let remaining_tasks = relay.stop();
    let read = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut [0])).await;
    assert_eq!(
        remaining_tasks, 0,
        "the server returned with live tasks in its runtime"
    );
    assert_eq!(
        read.unwrap().unwrap(),
        0,
        "the admitted socket stayed open after shutdown"
    );
}
