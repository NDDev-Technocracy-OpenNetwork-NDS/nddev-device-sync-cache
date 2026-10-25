//! Real-socket acceptance of the cache service against an isolated local upstream.
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use nddev_device_sync_cache::{
    config::Settings,
    domain::{Freshness, Route, Routes},
    telemetry::Identity,
};

#[derive(Default)]
struct Upstream {
    hits: AtomicUsize,
    channel_v2: AtomicBool,
    broken: AtomicBool,
}

async fn immutable(State(state): State<Arc<Upstream>>) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    (
        [(header::CONTENT_TYPE, "application/x-tar")],
        "immutable payload",
    )
        .into_response()
}

async fn slow(State(state): State<Arc<Upstream>>) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    "slow payload".into_response()
}

async fn channel(State(state): State<Arc<Upstream>>, headers: HeaderMap) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    if state.broken.load(Ordering::SeqCst) {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let version = if state.channel_v2.load(Ordering::SeqCst) {
        "v2"
    } else {
        "v1"
    };
    let etag = format!("\"{version}\"");
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(etag.as_str())
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    ([(header::ETAG, etag)], format!("channel {version}")).into_response()
}

async fn big() -> Response {
    vec![b'x'; 2 << 20].into_response()
}

async fn spawn(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    address
}

fn accept(path: &str) -> Option<Freshness> {
    if path.starts_with("/files/channel") {
        Some(Freshness::Revalidate)
    } else if path.starts_with("/files/") || path.starts_with("/hop/") {
        Some(Freshness::Immutable)
    } else {
        None
    }
}

struct Harness {
    base: String,
    upstream: Arc<Upstream>,
    root: PathBuf,
    _directory: tempfile::TempDir,
    client: reqwest::Client,
}

impl Harness {
    async fn start() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let upstream = Arc::new(Upstream::default());
        let origin = spawn(
            Router::new()
                .route("/files/immutable.bin", get(immutable))
                .route("/files/slow.bin", get(slow))
                .route("/files/channel.toml", get(channel))
                .route("/files/big.bin", get(big))
                .with_state(upstream.clone()),
        )
        .await;
        let redirector = spawn(
            Router::new()
                .route(
                    "/hop/allowed",
                    get(move || async move {
                        (
                            StatusCode::FOUND,
                            [(
                                header::LOCATION,
                                format!("http://{origin}/files/immutable.bin"),
                            )],
                        )
                    }),
                )
                .route(
                    "/hop/denied",
                    get(move || async move {
                        (
                            StatusCode::FOUND,
                            [(
                                header::LOCATION,
                                format!("http://localhost:{}/files/immutable.bin", origin.port()),
                            )],
                        )
                    }),
                ),
        )
        .await;
        let route = |prefix: &'static str, template: &'static str, upstream: String| Route {
            id: prefix.trim_start_matches('/'),
            prefix,
            template,
            upstream,
            accept,
        };
        let routes = Routes::new(
            vec![
                route("/origin", "/origin/{path}", format!("http://{origin}")),
                route(
                    "/redirector",
                    "/redirector/{path}",
                    format!("http://{redirector}"),
                ),
            ],
            vec!["127.0.0.1".into()],
            true,
        );
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("store");
        let settings = Settings::read(|name| match name {
            "NDS_CACHE_ROOT" => Some(root.to_string_lossy().into_owned()),
            "NDS_CACHE_CAPACITY_BYTES" => Some((1u64 << 30).to_string()),
            "NDS_CACHE_MAX_OBJECT_BYTES" => Some((1u64 << 20).to_string()),
            "NDS_CACHE_REVALIDATE_SECONDS" => Some("1".into()),
            _ => None,
        })
        .unwrap();
        let identity = Identity::read(&|_| None).unwrap();
        let app = nddev_device_sync_cache::app(&settings, identity, routes)
            .await
            .unwrap();
        let cache = spawn(nddev_device_sync_cache::http::router(app)).await;
        Self {
            base: format!("http://{cache}"),
            upstream,
            root,
            _directory: directory,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        }
    }

    async fn get(&self, path: &str) -> (u16, String, String) {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let served = response
            .headers()
            .get("x-nds-cache")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        (status, served, response.text().await.unwrap())
    }

    async fn ready(&self) -> (u16, serde_json::Value) {
        let response = self
            .client
            .get(format!("{}/v1/ready", self.base))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.text().await.unwrap();
        (status, serde_json::from_str(&body).unwrap())
    }

    fn hits(&self) -> usize {
        self.upstream.hits.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn immutable_objects_are_fetched_once_and_then_served_from_the_store() {
    let harness = Harness::start().await;
    assert_eq!(
        harness.get("/origin/files/immutable.bin").await,
        (200, "miss".into(), "immutable payload".into())
    );
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
