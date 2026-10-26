//! Real-socket acceptance of the cache service against an isolated local upstream.
mod support;

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use axum::http::{StatusCode, header};
use support::{Harness, INDEX_TYPES};

#[tokio::test]
async fn immutable_objects_are_fetched_once_and_then_served_from_the_store() {
    let harness = Harness::start().await;
    assert_eq!(
        harness.get("/origin/files/immutable.bin").await,
        (200, "miss".into(), "immutable payload".into())
    );
    harness.committed(1).await;
    assert_eq!(
        harness.get("/origin/files/immutable.bin").await,
        (200, "hit".into(), "immutable payload".into())
    );
    assert_eq!(harness.hits(), 1);
    let response = harness
        .client
        .head(format!("{}/origin/files/immutable.bin", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "17");
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/x-tar"
    );
}

#[tokio::test]
async fn concurrent_misses_share_one_upstream_fetch() {
    let harness = Arc::new(Harness::start().await);
    let requests = (0..8)
        .map(|_| {
            let harness = harness.clone();
            tokio::spawn(async move { harness.get("/origin/files/slow.bin").await })
        })
        .collect::<Vec<_>>();
    for request in requests {
        let (status, _, body) = request.await.unwrap();
        assert_eq!((status, body.as_str()), (200, "slow payload"));
    }
    assert_eq!(harness.hits(), 1);
}

#[tokio::test]
async fn mutable_objects_revalidate_refresh_and_fall_back_to_stale() {
    let harness = Harness::start().await;
    assert_eq!(
        harness.get("/origin/files/channel.toml").await,
        (200, "miss".into(), "channel v1".into())
    );
    assert_eq!(harness.get("/origin/files/channel.toml").await.1, "hit");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        harness.get("/origin/files/channel.toml").await,
        (200, "revalidated".into(), "channel v1".into())
    );
    harness.upstream.channel_v2.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        harness.get("/origin/files/channel.toml").await,
        (200, "miss".into(), "channel v2".into())
    );
    assert_eq!(harness.ready().await.1["upstream"], "ok");
    harness.upstream.broken.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        harness.get("/origin/files/channel.toml").await,
        (200, "stale".into(), "channel v2".into())
    );
    let (status, ready) = harness.ready().await;
    assert_eq!((status, ready["upstream"].as_str()), (200, Some("failing")));
}

#[tokio::test]
async fn oversized_and_missing_objects_are_not_cached() {
    let harness = Harness::start().await;
    assert_eq!(harness.get("/origin/files/big.bin").await.0, 502);
    assert_eq!(harness.get("/origin/files/big.bin").await.0, 502);
    assert_eq!(harness.get("/origin/files/missing.bin").await.0, 404);
    let mut leftovers = tokio::fs::read_dir(harness.root.join("tmp")).await.unwrap();
    assert!(leftovers.next_entry().await.unwrap().is_none());
}

#[tokio::test]
async fn requests_outside_the_allowlist_never_reach_an_upstream() {
    let harness = Harness::start().await;
    for path in [
        "/origin/other/file",
        "/origin/files/../secret",
        "/elsewhere/files/a",
        "/origin/files/a%2Fb",
    ] {
        assert_eq!(harness.get(path).await.0, 404, "{path}");
    }
    assert_eq!(harness.hits(), 0);
    let response = harness
        .client
        .post(format!("{}/origin/files/immutable.bin", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(harness.hits(), 0);
}

#[tokio::test]
async fn redirects_are_followed_only_to_allowlisted_hosts() {
    let harness = Harness::start().await;
    assert_eq!(
        harness.get("/redirector/hop/allowed").await,
        (200, "miss".into(), "immutable payload".into())
    );
    harness.committed(1).await;
    assert_eq!(harness.get("/redirector/hop/denied").await.0, 502);
}

#[tokio::test]
async fn readiness_reports_an_unwritable_store() {
    use std::os::unix::fs::PermissionsExt;
    let harness = Harness::start().await;
    let health = harness
        .client
        .get(format!("{}/v1/health", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let (status, ready) = harness.ready().await;
    assert_eq!((status, ready["upstream"].as_str()), (200, Some("unknown")));
    let tmp = harness.root.join("tmp");
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o500)).unwrap();
    let degraded = harness
        .client
        .get(format!("{}/v1/ready", harness.base))
        .send()
        .await
        .unwrap()
        .status();
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(degraded, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn clients_follow_a_download_before_it_completes() {
    let harness = Arc::new(Harness::start().await);
    let url = format!("{}/origin/files/stream.bin", harness.base);
    let started = std::time::Instant::now();
    let mut first = harness.client.get(&url).send().await.unwrap();
    assert_eq!(first.headers()["x-nds-cache"], "miss");
    let chunk = first.chunk().await.unwrap().unwrap();
    assert_eq!(&chunk[..], b"first-");
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "first bytes waited for the whole download"
    );
    let joined = harness.get("/origin/files/stream.bin").await;
    assert_eq!(joined, (200, "joined".into(), "first-second".into()));
    let mut rest = Vec::new();
    while let Some(chunk) = first.chunk().await.unwrap() {
        rest.extend_from_slice(&chunk);
    }
    assert_eq!(rest, b"second");
    harness.committed(1).await;
    assert_eq!(harness.get("/origin/files/stream.bin").await.1, "hit");
    assert_eq!(harness.hits(), 1);
}

#[tokio::test]
async fn a_disconnecting_client_does_not_abort_the_download() {
    let harness = Harness::start().await;
    let mut response = harness
        .client
        .get(format!("{}/origin/files/stream.bin", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(&response.chunk().await.unwrap().unwrap()[..], b"first-");
    drop(response);
    harness.committed(1).await;
    assert_eq!(
        harness.get("/origin/files/stream.bin").await,
        (200, "hit".into(), "first-second".into())
    );
    assert_eq!(harness.hits(), 1);
}

#[tokio::test]
async fn head_on_a_miss_answers_from_the_upstream_headers() {
    let harness = Harness::start().await;
    let started = std::time::Instant::now();
    let response = harness
        .client
        .head(format!("{}/origin/files/stream.bin", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "HEAD waited for the whole download"
    );
    harness.committed(1).await;
    assert_eq!(harness.hits(), 1);
}

#[tokio::test]
async fn a_failed_download_fails_its_clients_and_leaves_no_object() {
    let harness = Harness::start().await;
    let response = harness
        .client
        .get(format!("{}/origin/files/broken.bin", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.bytes().await.is_err(),
        "a partial download was delivered as complete"
    );
    assert_eq!(harness.objects().await, 0);
    let mut leftovers = tokio::fs::read_dir(harness.root.join("tmp")).await.unwrap();
    assert!(leftovers.next_entry().await.unwrap().is_none());
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
    assert_eq!(harness.hits(), 2);
}

#[tokio::test]
async fn the_index_configuration_follows_the_client_base() {
    let harness = Harness::start().await;
    let response = harness
        .client
        .get(format!("{}/crates-index/config.json", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let config: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(
        config,
        serde_json::json!({ "dl": format!("{}/crates/{{crate}}/{{crate}}-{{version}}.crate", harness.base) })
    );
    let foreign = harness
        .client
        .get(format!("{}/crates-index/config.json", harness.base))
        .header(header::HOST, "mirror.example:8740")
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_client_holding_the_object_gets_not_modified() {
    let harness = Harness::start().await;
    harness.get("/origin/files/immutable.bin").await;
    harness.committed(1).await;
    let url = format!("{}/origin/files/immutable.bin", harness.base);
    let stored = harness.client.get(&url).send().await.unwrap();
    let etag = stored.headers()[header::ETAG].to_str().unwrap().to_owned();
    assert!(etag.starts_with("\"sha256-"), "{etag}");
    let held = harness
        .client
        .get(&url)
        .header(header::IF_NONE_MATCH, &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(held.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(held.headers()[header::ETAG], etag.as_str());
    assert!(held.bytes().await.unwrap().is_empty());
    let other = harness
        .client
        .get(&url)
        .header(header::IF_NONE_MATCH, "\"sha256-other\"")
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), StatusCode::OK);
    assert_eq!(harness.hits(), 1);
}

#[tokio::test]
async fn index_documents_are_negotiated_and_link_back_to_the_client_base() {
    let harness = Harness::start().await;
    let url = format!("{}/registry/files/index.json", harness.base);
    let fetch = |accept: &'static str| {
        harness
            .client
            .get(&url)
            .header(header::ACCEPT, accept)
            .send()
    };
    let preferred = fetch("application/vnd.test.v1+json, application/json;q=0.5")
        .await
        .unwrap();
    assert_eq!(preferred.status(), StatusCode::OK);
    assert_eq!(preferred.headers()[header::CONTENT_TYPE], INDEX_TYPES[0]);
    assert_eq!(preferred.headers()[header::VARY], "accept");
    let etag = preferred.headers()[header::ETAG].clone();
    let document: serde_json::Value =
        serde_json::from_str(&preferred.text().await.unwrap()).unwrap();
    assert_eq!(document["files"][0]["sha256"], "unchanged");
    let link = document["files"][0]["url"].as_str().unwrap().to_owned();
    assert_eq!(
        link,
        format!("{}/registry/files/immutable.bin", harness.base)
    );
    let (status, served, body) = harness.get(&link[harness.base.len()..]).await;
    assert_eq!(
        (status, served.as_str(), body.as_str()),
        (200, "miss", "immutable payload")
    );

    let full = fetch("application/json").await.unwrap();
    assert_eq!(full.headers()[header::CONTENT_TYPE], INDEX_TYPES[1]);
    assert_eq!(
        fetch("text/html").await.unwrap().status(),
        StatusCode::NOT_ACCEPTABLE
    );

    let held = harness
        .client
        .get(&url)
        .header(header::ACCEPT, INDEX_TYPES[0])
        .header(header::IF_NONE_MATCH, etag.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(held.status(), StatusCode::NOT_MODIFIED);
    let port = harness.base.rsplit(':').next().unwrap();
    let elsewhere = harness
        .client
        .get(&url)
        .header(header::ACCEPT, INDEX_TYPES[0])
        .header(header::HOST, format!("localhost:{port}"))
        .header(header::IF_NONE_MATCH, etag)
        .send()
        .await
        .unwrap();
    assert_eq!(elsewhere.status(), StatusCode::OK);
    assert!(elsewhere.text().await.unwrap().contains(&format!(
        "\"http://localhost:{port}/registry/files/immutable.bin\""
    )));
    let foreign = harness
        .client
        .get(&url)
        .header(header::HOST, "mirror.example:8740")
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), StatusCode::BAD_REQUEST);

    let accepts = harness.upstream.accepts.lock().unwrap().clone();
    assert!(
        INDEX_TYPES
            .iter()
            .all(|media_type| accepts.iter().any(|accept| accept == media_type)),
        "{accepts:?}"
    );
    assert!(
        accepts
            .iter()
            .all(|accept| INDEX_TYPES.contains(&accept.as_str())),
        "{accepts:?}"
    );
}
