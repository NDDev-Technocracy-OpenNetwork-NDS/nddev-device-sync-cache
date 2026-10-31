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
    assert_eq!(
        harness.get("/oci/library/app/manifests/latest").await.0,
        200
    );
    let mismatch = format!("/oci/library/app/blobs/sha256:{}", support::WRONG_DIGEST);
    // A fast checksum rejection can abort HTTP before headers are flushed or
    // while the client reads the body. Neither boundary is promised by HTTP.
    if let Ok(response) = harness
        .client
        .get(format!("{}{mismatch}", harness.base))
        .send()
        .await
    {
        let _ = response.bytes().await;
    }
    let release = |digest: &str| {
        format!(
            "{}/github-releases/sha256/{digest}/example/tool/releases/download/v1/tool.bin",
            harness.base
        )
    };
    if let Ok(response) = harness
        .client
        .get(release(support::WRONG_DIGEST))
        .send()
        .await
    {
        let _ = response.bytes().await;
    }
    let response = harness
        .client
        .get(release(support::LAYER_DIGEST))
        .send()
        .await
        .unwrap();
    assert_eq!(response.bytes().await.unwrap().as_ref(), support::LAYER);
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
    let rejected_digest = named("cache.fill.failed")
        .into_iter()
        .find(|event| event["route"] == "/oci/{path}" && event["error.type"] == "digest_mismatch")
        .expect("the actual OCI checksum rejection must reach the canonical event stream");
    assert!(
        named("cache.fill.committed")
            .iter()
            .all(|event| event["trace_id"] != rejected_digest["trace_id"]),
        "a rejected checksum was committed"
    );
    assert!(
        named("cache.fill.failed")
            .iter()
            .any(|event| event["route"] == "/github-releases/{path}"
                && event["error.type"] == "digest_mismatch")
    );
    assert!(
        named("cache.fill.committed")
            .iter()
            .any(|event| event["route"] == "/github-releases/{path}")
    );
    let requests: Vec<&serde_json::Value> = named("cache.miss");
    for fill in named("cache.fill.committed")
        .into_iter()
        .chain(named("cache.fill.failed"))
    {
        assert!(
            fill["route"] == "/origin/{path}"
                || fill["route"] == "/oci/{path}"
                || fill["route"] == "/github-releases/{path}",
            "{fill}"
        );
        assert!(fill["duration_ms"].as_f64().is_some(), "{fill}");
        assert!(
            requests
                .iter()
                .any(|request| request["trace_id"] == fill["trace_id"]),
            "a fill is outside the trace of the request that started it"
        );
    }
    // The health counters equal the request events per route template and outcome.
    let health: serde_json::Value = serde_json::from_str(
        &harness
            .client
            .get(format!("{}/v1/health", harness.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    let mut evented = std::collections::BTreeMap::<(String, &str), u64>::new();
    for event in &events {
        let outcome = match event["event.name"].as_str().unwrap_or_default() {
            "cache.hit" => "hit",
            "cache.miss" => "miss",
            "cache.joined" => "joined",
            "cache.revalidated" => "revalidated",
            "cache.stale" => "stale",
            "cache.index.config" => "answered",
            "cache.rejected" => "rejected",
            "cache.failed" | "cache.store.read.failed" => "failed",
            _ => continue,
        };
        let route = event["route"].as_str().unwrap().to_owned();
        *evented.entry((route, outcome)).or_default() += 1;
    }
    let mut counted = std::collections::BTreeMap::<(String, &str), u64>::new();
    for (route, counts) in health["requests"]["routes"].as_object().unwrap() {
        for (outcome, count) in counts["outcomes"].as_object().unwrap() {
            if count.as_u64().unwrap() > 0 {
                counted.insert((route.clone(), outcome.as_str()), count.as_u64().unwrap());
            }
        }
        if named("cache.fill.committed")
            .iter()
            .any(|event| event["route"] == route.as_str())
        {
            assert!(counts["bytes_filled"].as_u64().unwrap() > 0, "{route}");
        }
    }
    assert_eq!(counted, evented);
    for event in &events {
        let text = event.to_string();
        assert!(
            !text.contains("127.0.0.1")
                && !text.contains("/files/")
                && !text.contains("example/tool")
                && !text.contains(support::LAYER_DIGEST),
            "an event carries an address or request path: {text}"
        );
    }
}
