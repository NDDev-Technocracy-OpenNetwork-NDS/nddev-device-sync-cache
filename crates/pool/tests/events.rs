//! Actual socket behavior and binary lifecycle, decoded by the canonical SDK.
use nddev_device_sync_cache_pool::{Pool, Probe, Upstream, serve, telemetry};
use opentelemetry::trace::TracerProvider;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn decode(bytes: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            nddev_device_sync_telemetry::decode_event(value.clone()).unwrap();
            assert_eq!(value["service.name"], "nds-forward-pool");
            let encoded = value.to_string();
            for excluded in ["127.0.0.1", "/v1/ready", "SSH-", "private-probe"] {
                assert!(!encoded.contains(excluded), "event leaked {excluded}");
            }
            value
        })
        .collect()
}

#[tokio::test]
async fn actual_pool_events_use_the_local_sdk_without_endpoint_details() {
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
    let capture = Capture::default();
    let (subscriber, control) = nddev_device_sync_telemetry::subscriber(
        telemetry::config(),
        capture.clone(),
        provider.tracer("pool-test"),
    )
    .unwrap();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = upstream.local_addr().unwrap();
    let responder = tokio::spawn(async move {
        loop {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut request = [0u8; 256];
            let read = stream.read(&mut request).await.unwrap();
            assert!(read > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
        }
    });
    let mut pool = Pool::new(
        vec![Upstream::new(address, false)],
        Probe::Http("/private-probe".into()),
    );
    pool.timeout = Duration::from_millis(300);
    let pool = Arc::new(pool);
    pool.check().await;
    assert!(pool.upstreams[0].healthy());
    responder.abort();
    let _ = responder.await;
    pool.check().await;
    assert!(!pool.upstreams[0].healthy());

    let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = front.local_addr().unwrap();
    let server = tokio::spawn(serve(front, pool));
    let mut client = TcpStream::connect(address).await.unwrap();
    let mut reply = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut reply))
        .await
        .unwrap()
        .unwrap();
    server.abort();
    let _ = server.await;

    assert_eq!(control.rejected_fields_or_events(), 0);
    let events = decode(&capture.0.lock().unwrap());
    for (name, outcome) in [
        ("pool.listen", "ready"),
        ("pool.upstream", "checked"),
        ("pool.connection", "unavailable"),
    ] {
        assert!(
            events
                .iter()
                .any(|event| event["event.name"] == name && event["outcome"] == outcome),
            "missing {name}/{outcome}"
        );
    }
    for available in [true, false] {
        assert!(
            events
                .iter()
                .any(|event| event["event.name"] == "pool.upstream"
                    && event["available"] == available)
        );
    }

    // An occupied loopback port makes the real binary report a startup failure
    // and its SDK lifecycle, without contacting any upstream or external service.
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_nds-forward-pool"))
        .args([
            "--listen",
            &occupied.local_addr().unwrap().to_string(),
            "--primary",
            "127.0.0.1:1",
            "--http-probe",
            "/private-probe",
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let events = decode(&result.stdout);
    for name in ["process.started", "pool.listen", "process.stopped"] {
        assert!(
            events.iter().any(|event| event["event.name"] == name),
            "missing {name}"
        );
    }
    assert!(
        events
            .iter()
            .any(|event| event["error.type"] == "bind_failed")
    );
}
