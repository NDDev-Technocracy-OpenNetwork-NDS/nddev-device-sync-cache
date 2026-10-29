mod support;

use nddev_device_sync_cache::telemetry::{self, Identity};
use nddev_device_sync_cache_egress::{Limits, Policy};
use opentelemetry::trace::TracerProvider;
use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

#[test]
fn the_real_binary_reports_a_typed_bind_failure_without_the_address() {
    use std::{
        net::TcpListener,
        process::{Command, Stdio},
        time::Instant,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nds-cache-egress"))
        .env_clear()
        .args(["--listen", &address])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut timed_out = false;
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    drop(listener);
    assert!(!timed_out, "owned startup child did not finish");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.len() <= 65536 && output.stderr.len() <= 65536);
    let text = String::from_utf8(output.stdout).unwrap();
    let events: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for event in &events {
        nddev_device_sync_telemetry::decode_event(event.clone()).unwrap();
    }
    assert!(
        events
            .iter()
            .any(|event| event["event.name"] == "egress.listen.failed"
                && event["error.type"] == "address_in_use")
    );
    assert!(
        !events
            .iter()
            .any(|event| event["event.name"] == "egress.listening")
    );
    assert!(!text.contains(&address));
    assert!(output.stderr.is_empty());
}

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn real_tunnel_outcomes_use_the_shared_sdk_and_never_include_destinations_or_bytes() {
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
    let capture = Capture::default();
    let identity = Identity::read(&|_| None).unwrap();
    let (subscriber, control) = nddev_device_sync_telemetry::subscriber(
        telemetry::config(&identity, None),
        capture.clone(),
        provider.tracer("egress-test"),
    )
    .unwrap();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let target = support::echo();
    let mut relay = support::relay(
        Policy {
            ports: vec![target.address.port()],
            loopback: true,
        },
        Limits::default(),
    );
    let mut stream = TcpStream::connect(relay.address).await.unwrap();
    stream
        .write_all(
            format!(
                "CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n",
                target.address.port()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut head = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !head.ends_with(b"\r\n\r\n") {
            head.push(stream.read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert!(head.starts_with(b"HTTP/1.1 200"));
    let payload = b"EGRESS_PRIVATE_TEST_PAYLOAD";
    stream.write_all(payload).await.unwrap();
    let mut echoed = vec![0; payload.len()];
    tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut echoed))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(echoed, payload);

    let mut refused = TcpStream::connect(relay.address).await.unwrap();
    refused
        .write_all(b"CONNECT secret-user@cache-fixture.invalid:443 HTTP/1.1\r\n\r\n")
        .await
        .unwrap();
    let mut answer = String::new();
    tokio::time::timeout(Duration::from_secs(3), refused.read_to_string(&mut answer))
        .await
        .unwrap()
        .unwrap();
    assert!(answer.starts_with("HTTP/1.1 400"));
    let mut ready = TcpStream::connect(relay.address).await.unwrap();
    ready.write_all(b"GET /v1/ready HTTP/1.1\r\nHost: readiness-private.invalid\r\nAuthorization: Bearer READINESS_TEST_SECRET\r\n\r\n").await.unwrap();
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(3), ready.read_to_string(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(relay.stop(), 0);
    drop(relay);
    drop(target);

    assert_eq!(control.rejected_fields_or_events(), 0);
    let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let events: Vec<serde_json::Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for event in &events {
        nddev_device_sync_telemetry::decode_event(event.clone()).unwrap();
        assert_eq!(event["trace_id"].as_str().unwrap().len(), 32);
        assert_eq!(event["span_id"].as_str().unwrap().len(), 16);
    }
    for name in [
        "egress.tunnel.opened",
        "egress.tunnel.refused",
        "egress.tunnel.finished",
        "egress.ready",
    ] {
        assert!(
            events.iter().any(|event| event["event.name"] == name),
            "missing {name}"
        );
    }
    assert!(
        events
            .iter()
            .any(|event| event["event.name"] == "egress.tunnel.finished"
                && event["outcome"] == "stopped"
                && event["error.type"] == "cancelled")
    );
    let opened = events
        .iter()
        .find(|event| event["event.name"] == "egress.tunnel.opened")
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| event["event.name"] == "egress.tunnel.finished"
                && event["trace_id"] == opened["trace_id"])
    );
    for secret in [
        "127.0.0.1",
        "secret-user",
        "cache-fixture.invalid",
        "EGRESS_PRIVATE_TEST_PAYLOAD",
        "readiness-private.invalid",
        "READINESS_TEST_SECRET",
    ] {
        assert!(
            !output.contains(secret),
            "private request data entered telemetry"
        );
    }
}
