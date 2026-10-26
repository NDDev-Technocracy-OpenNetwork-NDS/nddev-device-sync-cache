//! The events the service emits, decoded through the shared telemetry contract. A separate
//! test binary, because a thread's subscriber misses callsites that parallel tests reached first.
mod support;

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use nddev_device_sync_cache::telemetry::Identity;
use support::Harness;

/// Collects the SDK output of one test.
#[derive(Clone, Default)]
struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn emitted_events_satisfy_the_shared_telemetry_contract() {
    use opentelemetry::trace::TracerProvider;
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
    let capture = Capture::default();
    let identity = Identity::read(&|_| None).unwrap();
    let (subscriber, control) = nddev_device_sync_telemetry::subscriber(
        nddev_device_sync_cache::telemetry::config(&identity, None),
        capture.clone(),
        provider.tracer("cache-test"),
    )
    .unwrap();
    // The current-thread test runtime keeps every task on this thread's subscriber.
    let _subscriber = tracing::subscriber::set_default(subscriber);

    let harness = Arc::new(Harness::start().await);
    let first = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.get("/origin/files/stream.bin").await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(harness.get("/origin/files/stream.bin").await.1, "joined");
    assert_eq!(first.await.unwrap().1, "miss");
    harness.committed(1).await;
    assert_eq!(harness.get("/origin/files/stream.bin").await.1, "hit");
    let head = harness
        .client
        .head(format!("{}/origin/files/slow.bin", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(head.headers()["x-nds-cache"], "miss");
    harness.committed(2).await;
    assert!(
        harness
            .client
            .get(format!("{}/origin/files/broken.bin", harness.base))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .is_err()
    );
    assert_eq!(harness.get("/origin/files/missing.bin").await.0, 404);
    assert_eq!(harness.get("/elsewhere/files/a").await.0, 404);
    assert_eq!(harness.get("/crates-index/config.json").await.0, 200);
    let foreign = harness
        .client
        .get(format!("{}/crates-index/config.json", harness.base))
        .header(axum::http::header::HOST, "mirror.example")
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status().as_u16(), 400);
    let stored = harness
        .client
        .get(format!("{}/origin/files/stream.bin", harness.base))
        .send()
        .await
        .unwrap();
    let etag = stored.headers()[axum::http::header::ETAG].clone();
    let held = harness
        .client
        .get(format!("{}/origin/files/stream.bin", harness.base))
        .header(axum::http::header::IF_NONE_MATCH, etag)
        .send()
        .await
        .unwrap();
    assert_eq!(held.status().as_u16(), 304);
    let index = format!("{}/registry/files/index.json", harness.base);
    for (accept, status) in [(support::INDEX_TYPES[0], 200), ("text/html", 406)] {
        let response = harness
            .client
            .get(&index)
            .header(axum::http::header::ACCEPT, accept)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        response.bytes().await.unwrap();
    }
    assert_eq!(harness.get("/origin/files/channel.toml").await.1, "miss");
    harness.upstream.broken.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(harness.get("/origin/files/channel.toml").await.1, "stale");
    // The failed fill reports after its body error reaches the client.
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(control.rejected_fields_or_events(), 0);
    let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let events: Vec<serde_json::Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for event in &events {
        nddev_device_sync_telemetry::decode_event(event.clone())
            .unwrap_or_else(|_| panic!("event outside the contract: {}", event["event.name"]));
    }
    let named = |name: &str| -> Vec<&serde_json::Value> {
        events
            .iter()
            .filter(|event| event["event.name"] == name)
            .collect()
    };
    for name in [
        "cache.store.opened",
        "cache.index.config",
        "cache.miss",
        "cache.joined",
        "cache.hit",
        "cache.fill.committed",
        "cache.fill.failed",
        "cache.rejected",
        "cache.stale",
        "cache.upstream.failed",
        "cache.upstream.available",
        "cache.upstream.degraded",
    ] {
        assert!(!named(name).is_empty(), "{name} was not emitted");
    }
    let requests: Vec<&serde_json::Value> = named("cache.miss");
    for fill in named("cache.fill.committed")
        .into_iter()
        .chain(named("cache.fill.failed"))
    {
        assert_eq!(fill["route"], "/origin/{path}");
        assert!(fill["duration_ms"].as_f64().is_some(), "{fill}");
        assert!(
            requests
                .iter()
                .any(|request| request["trace_id"] == fill["trace_id"]),
            "a fill is outside the trace of the request that started it"
        );
    }
    for event in &events {
        let text = event.to_string();
        assert!(
            !text.contains("127.0.0.1") && !text.contains("/files/"),
            "an event carries an address or request path: {text}"
        );
    }
}
