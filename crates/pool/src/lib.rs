//! A loopback TCP listener in front of several redundant forwards to one service.
//!
//! A forward's local listener keeps accepting connections when its tunnel no longer carries
//! data, so a connect proves nothing. Each upstream is therefore probed end to end on a fixed
//! interval: the cache answers `GET /v1/ready` with `200`, an SSH server sends its banner.
//! New connections are spread round robin over the healthy primaries, so flows that a link
//! throttles one by one add up, and go to the healthy fallbacks in order only while no primary
//! is healthy. A connection is never moved once it carries bytes; a client that loses one
//! retries like after any connection failure. With no healthy upstream the listener still
//! tries every upstream in order, so a stale probe never blocks a working one.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub mod telemetry;

/// How an upstream proves that it carries data to the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// `GET <path>` answers `HTTP/1.x 200`.
    Http(String),
    /// The server's first bytes start with this banner, such as `SSH-`.
    Banner(String),
}

/// One redundant path to the service.
#[derive(Debug)]
pub struct Upstream {
    pub address: SocketAddr,
    /// A fallback takes connections only while no primary is healthy.
    pub fallback: bool,
    healthy: AtomicBool,
}

impl Upstream {
    pub fn new(address: SocketAddr, fallback: bool) -> Self {
        Self {
            address,
            fallback,
            healthy: AtomicBool::new(false),
        }
    }

    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }
}

/// The upstreams of one listener and the order a new connection tries them in.
#[derive(Debug)]
pub struct Pool {
    pub upstreams: Vec<Arc<Upstream>>,
    pub probe: Probe,
    pub interval: Duration,
    pub timeout: Duration,
    next: AtomicUsize,
}

impl Pool {
    pub fn new(upstreams: Vec<Upstream>, probe: Probe) -> Self {
        Self {
            upstreams: upstreams.into_iter().map(Arc::new).collect(),
            probe,
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(3),
            next: AtomicUsize::new(0),
        }
    }

    /// Healthy primaries starting at the next turn, then healthy fallbacks in order, then
    /// every other upstream in its configured order.
    pub fn order(&self) -> Vec<Arc<Upstream>> {
        let primaries: Vec<_> = self
            .upstreams
            .iter()
            .filter(|upstream| !upstream.fallback && upstream.healthy())
            .cloned()
            .collect();
        let mut order = Vec::with_capacity(self.upstreams.len());
        if !primaries.is_empty() {
            let start = self.next.fetch_add(1, Ordering::Relaxed) % primaries.len();
            order.extend(primaries[start..].iter().cloned());
            order.extend(primaries[..start].iter().cloned());
        }
        order.extend(
            self.upstreams
                .iter()
                .filter(|upstream| upstream.fallback && upstream.healthy())
                .cloned(),
        );
        for upstream in &self.upstreams {
            if !order.iter().any(|chosen| Arc::ptr_eq(chosen, upstream)) {
                order.push(upstream.clone());
            }
        }
        order
    }

    /// Probes every upstream once, concurrently, and records the outcome.
    pub async fn check(&self) {
        let mut probes = tokio::task::JoinSet::new();
        for upstream in &self.upstreams {
            let (upstream, probe, timeout) = (upstream.clone(), self.probe.clone(), self.timeout);
            probes.spawn(async move {
                let ok = tokio::time::timeout(timeout, carries(upstream.address, &probe))
                    .await
                    .unwrap_or(false);
                if upstream.healthy.swap(ok, Ordering::AcqRel) != ok {
                    tracing::info!(
                        module = "pool",
                        scope = "io",
                        event.name = "pool.upstream",
                        outcome = "checked",
                        available = ok,
                        route = if upstream.fallback {
                            "/forward/fallback"
                        } else {
                            "/forward/primary"
                        }
                    );
                }
            });
        }
        while probes.join_next().await.is_some() {}
    }
}

/// Whether the upstream carries a request to the service and back.
pub async fn carries(address: SocketAddr, probe: &Probe) -> bool {
    let Ok(mut stream) = TcpStream::connect(address).await else {
        return false;
    };
    match probe {
        Probe::Http(path) => {
            let request =
                format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
            if stream.write_all(request.as_bytes()).await.is_err() {
                return false;
            }
            let mut head = [0u8; 12];
            stream.read_exact(&mut head).await.is_ok()
                && (head.starts_with(b"HTTP/1.1 200") || head.starts_with(b"HTTP/1.0 200"))
        }
        Probe::Banner(banner) => {
            let mut head = vec![0u8; banner.len()];
            stream.read_exact(&mut head).await.is_ok() && head == banner.as_bytes()
        }
    }
}

/// Probes on the pool's interval for as long as the process runs.
pub async fn watch(pool: Arc<Pool>) {
    loop {
        pool.check().await;
        tokio::time::sleep(pool.interval).await;
    }
}

/// Accepts connections and splices each to the first upstream of `order` that connects.
pub async fn serve(listener: TcpListener, pool: Arc<Pool>) -> std::io::Result<()> {
    tracing::info!(
        module = "pool",
        scope = "io",
        event.name = "pool.listen",
        outcome = "ready"
    );
    loop {
        let (mut client, _) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::error!(module = "pool", scope = "io", event.name = "pool.listen", outcome = "error", error.type = "accept_failed");
                return Err(error);
            }
        };
        let pool = pool.clone();
        tokio::spawn(async move {
            for upstream in pool.order() {
                let connect = TcpStream::connect(upstream.address);
                if let Ok(Ok(mut server)) = tokio::time::timeout(pool.timeout, connect).await {
                    let _ = server.set_nodelay(true);
                    let _ = client.set_nodelay(true);
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                    return;
                }
            }
            tracing::warn!(module = "pool", scope = "io", event.name = "pool.connection", route = "/forward/{class}", outcome = "unavailable", error.type = "no_upstream");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(specs: &[(u16, bool, bool)]) -> Pool {
        let pool = Pool::new(
            specs
                .iter()
                .map(|(port, fallback, _)| {
                    Upstream::new(SocketAddr::from(([127, 0, 0, 1], *port)), *fallback)
                })
                .collect(),
            Probe::Http("/v1/ready".into()),
        );
        for (upstream, (_, _, healthy)) in pool.upstreams.iter().zip(specs) {
            upstream.healthy.store(*healthy, Ordering::Release);
        }
        pool
    }

    fn ports(order: &[Arc<Upstream>]) -> Vec<u16> {
        order
            .iter()
            .map(|upstream| upstream.address.port())
            .collect()
    }

    #[test]
    fn healthy_primaries_take_turns_and_fallbacks_wait() {
        let pool = pool(&[
            (1, false, true),
            (2, false, true),
            (3, false, false),
            (9, true, true),
        ]);
        assert_eq!(ports(&pool.order()), [1, 2, 9, 3]);
        assert_eq!(ports(&pool.order()), [2, 1, 9, 3]);
        assert_eq!(ports(&pool.order()), [1, 2, 9, 3]);
    }

    #[test]
    fn without_a_healthy_primary_the_healthy_fallback_comes_first() {
        let pool = pool(&[
            (1, false, false),
            (2, false, false),
            (8, true, false),
            (9, true, true),
        ]);
        assert_eq!(ports(&pool.order()), [9, 1, 2, 8]);
    }

    #[test]
    fn with_nothing_healthy_every_upstream_is_still_tried_in_order() {
        let pool = pool(&[(1, false, false), (9, true, false), (2, false, false)]);
        assert_eq!(ports(&pool.order()), [1, 9, 2]);
    }
}
