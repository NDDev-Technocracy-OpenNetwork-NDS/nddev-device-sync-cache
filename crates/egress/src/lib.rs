//! Loopback HTTPS egress relay for hosts whose own uplink cannot carry downloads.
//!
//! The relay runs on the cache host and listens on loopback only; devices reach it through their
//! existing forward, the same way they reach the cache. It answers only `CONNECT host:port` and
//! splices bytes, so TLS stays end to end between the client and the origin: nothing is
//! inspected, rewritten or stored. A destination must be a public address on an allowed port;
//! loopback, private, link-local, translation and other special-purpose ranges are refused.
//! This address policy does not authenticate the origin or replace host routing/firewall policy.
//! Events carry the
//! outcome only, never a host name or an address.

use nddev_device_sync_telemetry::{finish_operation_span, operation_span};
use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tracing::Instrument;

/// Which destinations the relay opens.
#[derive(Clone, Debug)]
pub struct Policy {
    /// Destination ports a tunnel may use; `443` by default.
    pub ports: Vec<u16>,
    /// Admits loopback destinations. Only fixtures set it; the binary never does.
    pub loopback: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            ports: vec![443],
            loopback: false,
        }
    }
}

/// Bounds of one relay.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The most bytes a request head may hold.
    pub head_bytes: usize,
    /// How long a client may take to send its request head.
    pub head_timeout: Duration,
    /// How long resolving and connecting a destination may take together.
    pub connect_timeout: Duration,
    /// Open tunnels at most; one more is answered `503`.
    pub connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            head_bytes: 8 * 1024,
            head_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(15),
            connections: 512,
        }
    }
}

/// Why a request was not tunnelled; each maps to one status line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("malformed request")]
    Malformed,
    #[error("only CONNECT is relayed")]
    Method,
    #[error("request head too large")]
    TooLarge,
    #[error("request head not received in time")]
    Timeout,
    #[error("destination port not allowed")]
    Port,
    #[error("destination is not a public address")]
    Forbidden,
    #[error("destination unreachable")]
    Unreachable,
    #[error("relay at capacity")]
    Capacity,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::Method => "method",
            Self::TooLarge => "too_large",
            Self::Timeout => "timeout",
            Self::Port => "port",
            Self::Forbidden => "forbidden",
            Self::Unreachable => "unreachable",
            Self::Capacity => "capacity",
        }
    }

    fn status(self) -> &'static str {
        match self {
            Self::Malformed => "400 Bad Request",
            Self::Method => "405 Method Not Allowed",
            Self::TooLarge => "431 Request Header Fields Too Large",
            Self::Timeout => "408 Request Timeout",
            Self::Port | Self::Forbidden => "403 Forbidden",
            Self::Unreachable => "502 Bad Gateway",
            Self::Capacity => "503 Service Unavailable",
        }
    }
}

/// The destination of a `CONNECT` request head, which must end the head exactly.
pub fn parse_connect(head: &[u8]) -> Result<(String, u16), Refusal> {
    let text = std::str::from_utf8(head).map_err(|_| Refusal::Malformed)?;
    let line = text.split("\r\n").next().ok_or(Refusal::Malformed)?;
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Refusal::Malformed);
    };
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(Refusal::Malformed);
    }
    if method != "CONNECT" {
        return Err(Refusal::Method);
    }
    let (host, port) = target.rsplit_once(':').ok_or(Refusal::Malformed)?;
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    let port: u16 = port.parse().map_err(|_| Refusal::Malformed)?;
    let valid = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'));
    if !valid || port == 0 {
        return Err(Refusal::Malformed);
    }
    Ok((host.to_ascii_lowercase(), port))
}

/// Whether a destination is admitted by the relay's public-address policy. IPv6 is native
/// global unicast only, excluding protocol assignments, transition and documentation ranges.
/// See IANA's IPv4/IPv6 Special-Purpose Address Registries and IPv6 Global Unicast Address Space:
/// <https://www.iana.org/assignments/iana-ipv6-special-registry/>.
pub fn public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

fn public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 240)
}

fn public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    // Admit 2000::/3 only. This excludes mapped/compatible addresses and the standard
    // NAT64 prefixes; special/transition ranges inside the block are excluded below.
    (segments[0] & 0xe000) == 0x2000
        && !((segments[0] == 0x2001 && (segments[1] & 0xfe00) == 0)
            || (segments[0] == 0x2001 && segments[1] == 0x0db8)
            || segments[0] == 0x2002
            || (segments[0] == 0x3fff && (segments[1] & 0xf000) == 0))
}

/// Owns at most `connections` tunnel tasks and one serial capacity response. Shutdown closes
/// admission, drops the listener and joins every tunnel before returning. The request-head
/// timeout also bounds writing a protocol response; established tunnels last until peer close
/// or shutdown. No destination, client address or payload enters telemetry.
pub async fn serve(
    listener: TcpListener,
    policy: Policy,
    limits: Limits,
    shutdown: impl Future<Output = ()>,
) -> std::io::Result<()> {
    let policy = Arc::new(policy);
    let mut tasks = JoinSet::new();
    tokio::pin!(shutdown);
    let mut result = loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break Ok(()),
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if joined.is_some_and(|result| result.is_err()) {
                    break Err(std::io::Error::other("tunnel_task_failed"));
                }
            }
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(error) => break Err(error),
                };
                let span = match operation_span("cache", "transport") {
                    Ok(span) => span,
                    Err(_) => break Err(std::io::Error::other("telemetry_invalid")),
                };
                if tasks.len() >= limits.connections {
                    // No task is spawned for a refusal, so slow readers cannot evade capacity.
                    tokio::select! {
                        biased;
                        _ = &mut shutdown => break Ok(()),
                        _ = refuse(stream, Refusal::Capacity, limits.head_timeout).instrument(span.clone()) => {
                            finish_operation_span(&span, true);
                        }
                    }
                } else {
                    let policy = policy.clone();
                    // Construct before spawn: even an unpolled/cancelled task has a terminal event.
                    let terminal = Terminal::new(span.clone());
                    tasks.spawn(async move {
                        handle(stream, &policy, limits, terminal).await;
                    }.instrument(span));
                }
            }
        }
    };
    drop(listener);
    tasks.abort_all();
    while let Some(joined) = tasks.join_next().await {
        if joined.is_err_and(|error| !error.is_cancelled()) && result.is_ok() {
            result = Err(std::io::Error::other("tunnel_task_failed"));
        }
    }
    result
}

struct Terminal {
    started: Instant,
    finished: bool,
    span: tracing::Span,
}

impl Terminal {
    fn new(span: tracing::Span) -> Self {
        Self {
            started: Instant::now(),
            finished: false,
            span,
        }
    }

    fn finish(&mut self, outcome: &'static str, error: &'static str) {
        finish_operation_span(&self.span, outcome == "error");
        self.span.in_scope(|| {
            tracing::info!(module = "cache", event.name = "egress.tunnel.finished", outcome,
                error.type = error, duration_ms = self.started.elapsed().as_secs_f64() * 1000.0);
        });
        self.finished = true;
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if !self.finished {
            self.finish("stopped", "cancelled");
        }
    }
}

async fn handle(mut client: TcpStream, policy: &Policy, limits: Limits, mut terminal: Terminal) {
    let destination = match tokio::time::timeout(
        limits.head_timeout,
        read_head(&mut client, limits.head_bytes),
    )
    .await
    {
        Err(_) => Err(Refusal::Timeout),
        Ok(head) => head.and_then(|head| parse_connect(&head)),
    };
    let upstream = match destination {
        Ok((host, port)) => open(&host, port, policy, limits).await,
        Err(refusal) => Err(refusal),
    };
    let mut upstream = match upstream {
        Ok(upstream) => upstream,
        Err(refusal) => {
            refuse(client, refusal, limits.head_timeout).await;
            terminal.finish("error", refusal.code());
            return;
        }
    };
    match tokio::time::timeout(
        limits.head_timeout,
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n"),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            terminal.finish("error", "io");
            return;
        }
        Err(_) => {
            terminal.finish("error", "timeout");
            return;
        }
    }
    tracing::info!(
        module = "cache",
        event.name = "egress.tunnel.opened",
        outcome = "ok"
    );
    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok(_) => terminal.finish("ok", "none"),
        Err(_) => terminal.finish("error", "io"),
    }
}

/// Reads up to the blank line that ends the head; a tunnel's own bytes follow only after `200`.
async fn read_head(client: &mut TcpStream, limit: usize) -> Result<Vec<u8>, Refusal> {
    let mut head = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= limit {
            return Err(Refusal::TooLarge);
        }
        match client.read(&mut byte).await {
            Ok(1) => head.push(byte[0]),
            _ => return Err(Refusal::Malformed),
        }
    }
    Ok(head)
}

async fn open(
    host: &str,
    port: u16,
    policy: &Policy,
    limits: Limits,
) -> Result<TcpStream, Refusal> {
    if !policy.ports.contains(&port) {
        return Err(Refusal::Port);
    }
    let attempt = async {
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| Refusal::Unreachable)?
            .collect();
        let allowed: Vec<SocketAddr> = addresses
            .into_iter()
            .filter(|address| {
                public(address.ip()) || (policy.loopback && address.ip().is_loopback())
            })
            .collect();
        if allowed.is_empty() {
            return Err(Refusal::Forbidden);
        }
        for address in allowed {
            if let Ok(stream) = TcpStream::connect(address).await {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
        }
        Err(Refusal::Unreachable)
    };
    tokio::time::timeout(limits.connect_timeout, attempt)
        .await
        .map_err(|_| Refusal::Unreachable)?
}

async fn refuse(mut client: TcpStream, refusal: Refusal, timeout: Duration) {
    tracing::warn!(module = "cache", event.name = "egress.tunnel.refused", outcome = "error", error.type = refusal.code());
    let response = format!(
        "HTTP/1.1 {}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        refusal.status()
    );
    let _ = tokio::time::timeout(timeout, async {
        let _ = client.write_all(response.as_bytes()).await;
        let _ = client.shutdown().await;
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_well_formed_connect_names_a_destination() {
        assert_eq!(
            parse_connect(b"CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\n\r\n"),
            Ok(("github.com".to_owned(), 443))
        );
        assert_eq!(
            parse_connect(b"CONNECT [2606:4700::1]:443 HTTP/1.1\r\n\r\n"),
            Ok(("2606:4700::1".to_owned(), 443))
        );
        for (head, refusal) in [
            (
                &b"GET http://example.com/ HTTP/1.1\r\n\r\n"[..],
                Refusal::Method,
            ),
            (b"CONNECT example.com HTTP/1.1\r\n\r\n", Refusal::Malformed),
            (
                b"CONNECT example.com:0 HTTP/1.1\r\n\r\n",
                Refusal::Malformed,
            ),
            (
                b"CONNECT exa mple.com:443 HTTP/1.1\r\n\r\n",
                Refusal::Malformed,
            ),
            (
                b"CONNECT user@example.com:443 HTTP/1.1\r\n\r\n",
                Refusal::Malformed,
            ),
            (
                b"CONNECT example.com:443 SPDY/3\r\n\r\n",
                Refusal::Malformed,
            ),
            (
                b"CONNECT example.com:443 HTTP/1.bad\r\n\r\n",
                Refusal::Malformed,
            ),
            (b"\xff\xfe\r\n\r\n", Refusal::Malformed),
        ] {
            assert_eq!(parse_connect(head), Err(refusal), "{head:?}");
        }
    }

    #[test]
    fn only_public_addresses_are_destinations() {
        for public_address in [
            "140.82.121.4",
            "185.199.109.133",
            "2606:4700::1111",
            "2001:4860:4860::8888",
        ] {
            assert!(public(public_address.parse().unwrap()), "{public_address}");
        }
        for special in [
            "127.0.0.1",
            "10.1.2.3",
            "172.19.0.1",
            "192.168.31.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "192.0.0.8",
            "192.88.99.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:8.8.8.8",
            "::127.0.0.1",
            "64:ff9b::a00:1",
            "64:ff9b:1::a00:1",
            "100::1",
            "100:0:0:1::1",
            "2001:2::1",
            "2001:20::1",
            "2002:a00:1::1",
            "3fff::1",
            "5f00::1",
            "ff02::1",
        ] {
            assert!(!public(special.parse().unwrap()), "{special}");
        }
    }
}
