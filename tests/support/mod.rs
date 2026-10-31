//! Shared real-socket harness: an isolated local upstream and a cache service in front of it.
#![allow(
    dead_code,
    reason = "each test binary uses a part of the shared harness"
)]
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
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
    domain::{Freshness, Index, Inferred, Route, Routes, verbatim},
    telemetry::Identity,
};

#[derive(Default)]
pub struct Upstream {
    pub hits: AtomicUsize,
    pub channel_v2: AtomicBool,
    pub broken: AtomicBool,
    /// The `Accept` values of index document requests, in arrival order.
    pub accepts: Mutex<Vec<String>>,
    /// Tokens the test registry issued, and the `Accept` values of its manifest requests.
    pub tokens: AtomicUsize,
    pub manifest_accepts: Mutex<Vec<String>>,
    pub release_credentials: AtomicBool,
}

/// The test registry's layer and the SHA-256 hex digest of it, and a digest its bytes lack.
pub const LAYER: &[u8] = b"layer bytes";
pub const LAYER_DIGEST: &str = "3997bfa39738f279d788e49d0cc743d02a512c8855f798b3ee1c0daf589a6b9a";
pub const WRONG_DIGEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const REGISTRY_TOKEN: &str = "anonymous-test-token";

/// A registry that issues anonymous tokens and refuses manifests without one.
async fn registry_token(State(state): State<Arc<Upstream>>) -> Response {
    state.tokens.fetch_add(1, Ordering::SeqCst);
    axum::Json(serde_json::json!({ "token": REGISTRY_TOKEN, "expires_in": 300 })).into_response()
}

async fn registry_manifest(State(state): State<Arc<Upstream>>, headers: HeaderMap) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        == Some(&format!("Bearer {REGISTRY_TOKEN}"));
    if !authorized {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let challenge = format!(
            r#"Bearer realm="http://{host}/token",service="test",scope="repository:library/app:pull""#
        );
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, challenge)],
        )
            .into_response();
    }
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    state
        .manifest_accepts
        .lock()
        .unwrap()
        .push(accept.to_owned());
    let manifest =
        format!(r#"{{"schemaVersion":2,"layers":[{{"digest":"sha256:{LAYER_DIGEST}"}}]}}"#);
    (
        [(
            header::CONTENT_TYPE,
            "application/vnd.oci.image.manifest.v1+json",
        )],
        manifest,
    )
        .into_response()
}

/// Blobs redirect to storage, as registries do; the wrong digest serves the layer anyway.
async fn registry_blob(axum::extract::Path(digest): axum::extract::Path<String>) -> Response {
    (
        StatusCode::TEMPORARY_REDIRECT,
        [(header::LOCATION, format!("/storage/{digest}"))],
    )
        .into_response()
}

async fn registry_storage(State(state): State<Arc<Upstream>>) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    LAYER.into_response()
}

async fn release_asset(
    State(state): State<Arc<Upstream>>,
    axum::extract::Path((_tag, asset)): axum::extract::Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    state.release_credentials.store(
        headers.contains_key(header::AUTHORIZATION) || headers.contains_key(header::COOKIE),
        Ordering::SeqCst,
    );
    if state.broken.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let host = headers[header::HOST].to_str().unwrap();
    let target = if asset == "denied.bin" {
        format!(
            "http://localhost:{}/storage/release",
            host.rsplit(':').next().unwrap()
        )
    } else {
        format!("http://{host}/storage/release")
    };
    (StatusCode::FOUND, [(header::LOCATION, target)]).into_response()
}

/// The representations of the test index document, preferred first.
pub const INDEX_TYPES: &[&str] = &["application/vnd.test.v1+json", "application/json"];
/// The product of a client that derives the index mirror link itself, in the first type.
pub const INFERRING: &str = "Inferring/";

async fn immutable(State(state): State<Arc<Upstream>>) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    (
        [(header::CONTENT_TYPE, "application/x-tar")],
        "immutable payload",
    )
        .into_response()
}

/// A signed note in the checksum database's form; the cache passes it through byte for byte.
pub const SUMDB_NOTE: &str = "go.sum database tree\n42\nAAAA=\n\n— sum.golang.org Az3grnmrIUEDFqHzAzNHhTz3c5bbg6F4yPtBzwEDvwwJ/8cbxFn9JdLW5LhvfLWnwCyAqMtX7lRTP/Ur1aszc0bQaQw=\n";

async fn sumdb_latest(State(state): State<Arc<Upstream>>) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        SUMDB_NOTE,
    )
        .into_response()
}

async fn sumdb_lookup(State(state): State<Arc<Upstream>>) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    let record = format!("40\ngithub.com/google/uuid v1.6.0 h1:x\n\n{SUMDB_NOTE}");
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        record,
    )
        .into_response()
}

async fn sumdb_tile(State(state): State<Arc<Upstream>>) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    (
        [(header::CONTENT_TYPE, "application/octet-stream")],
        vec![7u8; 8192],
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

/// Sends a first chunk at once and the rest after a pause, without a declared length.
async fn stream(State(state): State<Arc<Upstream>>) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    let chunks = futures_util::stream::unfold(0, |step| async move {
        match step {
            0 => Some((
                Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"first-")),
                1,
            )),
            1 => {
                tokio::time::sleep(Duration::from_millis(800)).await;
                Some((Ok(bytes::Bytes::from_static(b"second")), 2))
            }
            _ => None,
        }
    });
    axum::body::Body::from_stream(chunks).into_response()
}

/// Sends a first chunk, then fails the body.
async fn broken_stream(State(state): State<Arc<Upstream>>) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    let chunks = futures_util::stream::unfold(0, |step| async move {
        match step {
            0 => Some((Ok(bytes::Bytes::from_static(b"partial")), 1)),
            1 => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Some((Err(std::io::Error::other("upstream reset")), 2))
            }
            _ => None,
        }
    });
    axum::body::Body::from_stream(chunks).into_response()
}

/// An index document in the representation the request names, linking one download by the
/// absolute URL of this upstream, in its file list and as its mirror.
async fn index(State(state): State<Arc<Upstream>>, headers: HeaderMap) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    let text = |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let (accept, host) = (text(header::ACCEPT), text(header::HOST));
    state.accepts.lock().unwrap().push(accept.clone());
    let document = serde_json::json!({
        "files": [{"url": format!("http://{host}/files/immutable.bin"), "sha256": "unchanged"}],
        "mirror": {"url": format!("http://{host}/files/immutable.bin")},
        "representation": accept,
    });
    ([(header::CONTENT_TYPE, accept)], document.to_string()).into_response()
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

fn registry_accept(path: &str) -> Option<Freshness> {
    match path {
        "/files/index.json" => Some(Freshness::Revalidate),
        "/files/immutable.bin" => Some(Freshness::Immutable),
        _ => None,
    }
}

fn oci_accept(path: &str) -> Option<Freshness> {
    if path.ends_with("/manifests/latest") {
        Some(Freshness::Revalidate)
    } else if path.contains("/blobs/sha256:") {
        Some(Freshness::Immutable)
    } else {
        None
    }
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

pub struct Harness {
    pub base: String,
    pub upstream: Arc<Upstream>,
    pub root: PathBuf,
    _directory: tempfile::TempDir,
    pub client: reqwest::Client,
}

impl Harness {
    pub async fn start() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let upstream = Arc::new(Upstream::default());
        let origin = spawn(
            Router::new()
                .route("/files/immutable.bin", get(immutable))
                .route("/files/slow.bin", get(slow))
                .route("/files/channel.toml", get(channel))
                .route("/files/big.bin", get(big))
                .route("/files/stream.bin", get(stream))
                .route("/files/broken.bin", get(broken_stream))
                .route("/files/index.json", get(index))
                .route("/sumdb/latest", get(sumdb_latest))
                .route(
                    "/sumdb/lookup/github.com/google/uuid@v1.6.0",
                    get(sumdb_lookup),
                )
                .route("/sumdb/tile/8/0/x001/234", get(sumdb_tile))
                .route("/token", get(registry_token))
                .route("/v2/library/app/manifests/latest", get(registry_manifest))
                .route("/v2/library/app/blobs/{digest}", get(registry_blob))
                .route("/storage/{digest}", get(registry_storage))
                .route(
                    "/example/tool/releases/download/{tag}/{asset}",
                    get(release_asset),
                )
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
            canonical: verbatim,
            accept,
            index: None,
            oci: false,
            sha256_prefix: false,
            answers: &[],
        };
        let routes = Routes::new(
            vec![
                route("/origin", "/origin/{path}", format!("http://{origin}")),
                route(
                    "/redirector",
                    "/redirector/{path}",
                    format!("http://{redirector}"),
                ),
                // The checksum database's own rules, over a local upstream.
                Route {
                    accept: nddev_device_sync_cache::domain::go_sumdb,
                    answers: &["/supported"],
                    ..route(
                        "/gosumdb",
                        "/gosumdb/{path}",
                        format!("http://{origin}/sumdb"),
                    )
                },
                Route {
                    accept: oci_accept,
                    oci: true,
                    ..route("/oci", "/oci/{path}", format!("http://{origin}/v2"))
                },
                Route {
                    accept: nddev_device_sync_cache::domain::github_release,
                    sha256_prefix: true,
                    ..route(
                        "/github-releases",
                        "/github-releases/{path}",
                        format!("http://{origin}"),
                    )
                },
                Route {
                    accept: registry_accept,
                    index: Some(Index {
                        media_types: INDEX_TYPES,
                        links: vec![format!("http://{origin}/")],
                        mount: "/registry/",
                        inferred: Some(Inferred {
                            client: INFERRING,
                            media_type: INDEX_TYPES[0],
                            path: &["mirror", "url"],
                            link: format!("http://{origin}/"),
                        }),
                    }),
                    ..route("/registry", "/registry/{path}", format!("http://{origin}"))
                },
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

    pub async fn get(&self, path: &str) -> (u16, String, String) {
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

    pub async fn ready(&self) -> (u16, serde_json::Value) {
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

    /// The `/v1/health` request counters of one route template.
    pub async fn counted(&self, route: &str) -> serde_json::Value {
        let response = self
            .client
            .get(format!("{}/v1/health", self.base))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_str(&response.text().await.unwrap()).unwrap();
        body["requests"]["routes"][route].clone()
    }

    pub async fn objects(&self) -> u64 {
        let response = self
            .client
            .get(format!("{}/v1/health", self.base))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_str(&response.text().await.unwrap()).unwrap();
        body["store_objects"].as_u64().unwrap()
    }

    /// Waits until the store holds `count` objects, as background downloads commit.
    pub async fn committed(&self, count: u64) {
        for _ in 0..100 {
            if self.objects().await == count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("store did not reach {count} objects");
    }

    pub fn hits(&self) -> usize {
        self.upstream.hits.load(Ordering::SeqCst)
    }
}
