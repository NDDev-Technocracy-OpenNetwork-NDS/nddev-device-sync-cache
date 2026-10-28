//! The pool against real loopback sockets: live, frozen and dead forwards.

use nddev_device_sync_cache_pool::{Pool, Probe, Upstream, carries, serve};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// A live forward: answers the probe with `200` and every other request with its own name.
async fn live(name: &'static str, served: Arc<AtomicUsize>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let served = served.clone();
            tokio::spawn(async move {
                let mut request = [0u8; 256];
                let read = stream.read(&mut request).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&request[..read]).to_string();
                if !text.contains("/v1/ready") {
                    served.fetch_add(1, Ordering::Relaxed);
                }
                let body = if text.contains("/v1/ready") {
                    "{}"
                } else {
                    name
                };
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    address
}

/// A frozen forward: its listener accepts but nothing ever comes back, like a dead tunnel.
async fn frozen() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    address
}

/// A dead forward: nothing listens.
async fn dead() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

async fn front(upstreams: Vec<Upstream>) -> (SocketAddr, Arc<Pool>) {
    let mut pool = Pool::new(upstreams, Probe::Http("/v1/ready".into()));
    pool.timeout = Duration::from_millis(300);
    let pool = Arc::new(pool);
    pool.check().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, pool.clone()));
    (address, pool)
}

async fn get(front: SocketAddr) -> String {
    let mut stream = TcpStream::connect(front).await.unwrap();
    stream.write_all(b"GET /x HTTP/1.1\r\n\r\n").await.unwrap();
    let mut answer = String::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_string(&mut answer))
        .await
        .expect("the pool left the request hanging")
        .unwrap();
    answer
        .rsplit("\r\n\r\n")
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn connections_spread_over_live_primaries_and_skip_frozen_and_dead_ones() {
    let (a, b) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let upstreams = vec![
        Upstream::new(live("a", a.clone()).await, false),
        Upstream::new(frozen().await, false),
        Upstream::new(dead().await, false),
        Upstream::new(live("b", b.clone()).await, false),
    ];
    let (front, pool) = front(upstreams).await;
    let health: Vec<bool> = pool
        .upstreams
        .iter()
        .map(|upstream| upstream.healthy())
        .collect();
    assert_eq!(health, [true, false, false, true]);
    for _ in 0..10 {
        let body = get(front).await;
        assert!(body == "a" || body == "b", "{body}");
    }
    assert_eq!(a.load(Ordering::Relaxed), 5);
    assert_eq!(b.load(Ordering::Relaxed), 5);
}

#[tokio::test]
async fn the_fallback_serves_only_while_no_primary_is_live() {
    let fallback = Arc::new(AtomicUsize::new(0));
    let primary = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let primary_address = primary.local_addr().unwrap();
    drop(primary);
    let upstreams = vec![
        Upstream::new(primary_address, false),
        Upstream::new(live("fallback", fallback.clone()).await, true),
    ];
    let (front, pool) = front(upstreams).await;
    assert_eq!(get(front).await, "fallback");

    // The primary comes back on its port: after the next probe it takes the traffic again.
    let served = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind(primary_address).await.unwrap();
    let counter = served.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let counter = counter.clone();
            tokio::spawn(async move {
                let mut request = [0u8; 256];
                let read = stream.read(&mut request).await.unwrap_or(0);
                if !String::from_utf8_lossy(&request[..read]).contains("/v1/ready") {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\nprimary")
                    .await;
            });
        }
    });
    pool.check().await;
    assert_eq!(get(front).await, "primary");
    assert_eq!(fallback.load(Ordering::Relaxed), 1);
    assert_eq!(served.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn a_banner_probe_tells_a_live_ssh_server_from_a_frozen_tunnel() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ssh = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = stream.write_all(b"SSH-2.0-OpenSSH_10.2\r\n").await;
        }
    });
    let banner = Probe::Banner("SSH-".into());
    assert!(carries(ssh, &banner).await);
    let frozen = frozen().await;
    assert!(
        !tokio::time::timeout(Duration::from_millis(300), carries(frozen, &banner))
            .await
            .unwrap_or(false)
    );
    assert!(!carries(dead().await, &banner).await);
}
