//! HTTP adapter: protocol routes, health, readiness and the AGPL source offer.
use std::{sync::Arc, time::Instant};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::StreamExt;
use serde::Serialize;
use tokio_util::io::ReaderStream;
use tracing::Instrument;

use crate::{
    REPOSITORY, SOURCE_COMMIT,
    cache::{Cache, Delivery, Filling, Object, ServeError, Served},
    counters::{Outcome, Snapshot},
    domain, store,
    telemetry::Identity,
};

pub struct App {
    pub cache: Cache,
    pub identity: Identity,
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/ready", get(ready))
        .route("/source", get(source))
        .route("/crates-index/config.json", get(crates_config))
        .route("/v2/", get(registry_ping))
        .route("/v2", get(registry_ping))
        .fallback(serve)
        .with_state(app)
}

#[derive(Serialize)]
struct HealthResponse<'a> {
    status: &'static str,
    service: &'static str,
    version: &'a str,
    channel: &'a str,
    standards_release: &'a str,
    source_commit: &'static str,
    store_objects: usize,
    store_bytes: u64,
    store_capacity: u64,
    requests: Snapshot,
}

async fn health(State(app): State<Arc<App>>) -> Response {
    let usage = app.cache.store().usage();
    Json(HealthResponse {
        status: "ok",
        service: "nddev-device-sync-cache",
        version: &app.identity.version,
        channel: &app.identity.channel,
        standards_release: &app.identity.standards_release,
        source_commit: SOURCE_COMMIT,
        store_objects: usage.objects,
        store_bytes: usage.bytes,
        store_capacity: usage.capacity,
        requests: app.cache.counters().snapshot(),
    })
    .into_response()
}

#[derive(Serialize)]
struct ReadyResponse {
    status: &'static str,
    store: &'static str,
    upstream: &'static str,
}

async fn ready(State(app): State<Arc<App>>) -> Response {
    let upstream = app.cache.upstream_state().as_str();
    if app.cache.store().ready().await {
        Json(ReadyResponse {
            status: "ready",
            store: "ok",
            upstream,
        })
        .into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ReadyResponse {
                status: "degraded",
                store: "unavailable",
                upstream,
            }),
        )
            .into_response()
    }
}

async fn source() -> Response {
    Json(serde_json::json!({
        "source_url": format!("https://github.com/{REPOSITORY}/tree/{SOURCE_COMMIT}"),
        "license": "AGPL-3.0-only",
    }))
    .into_response()
}

/// The OCI distribution API check that registry clients make before pulling; the cache asks for
/// no credentials.
async fn registry_ping() -> Response {
    (
        [(DISTRIBUTION_API, HeaderValue::from_static("registry/2.0"))],
        Json(serde_json::json!({})),
    )
        .into_response()
}

const DISTRIBUTION_API: &str = "docker-distribution-api-version";

/// The crates.io sparse index configuration, generated for the loopback base address the client
/// used, so that crate downloads come back through this cache.
async fn crates_config(
    State(app): State<Arc<App>>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let started = Instant::now();
    let route = domain::CRATES_CONFIG_TEMPLATE;
    let counters = app.cache.counters();
    let span = nddev_device_sync_telemetry::http_server_span(method.as_str(), route)
        .unwrap_or_else(|_| tracing::Span::none());
    let finished = span.clone();
    let response = async {
        let duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        let config = headers
            .get(header::HOST)
            .and_then(|host| host.to_str().ok())
            .and_then(domain::crates_index_config);
        match config {
            Some(config) => {
                tracing::info!(module = "cache", event.name = "cache.index.config", method = method.as_str(), route, status = 200, duration_ms, outcome = "ok");
                counters.outcome(route, Outcome::Answered);
                if method != Method::HEAD {
                    counters.sent(route, config.len() as u64);
                }
                ([(header::CONTENT_TYPE, "application/json")], config).into_response()
            }
            None => {
                tracing::info!(module = "cache", event.name = "cache.rejected", method = method.as_str(), route, status = 400, duration_ms, error.type = "host_not_loopback", outcome = "rejected");
                counters.outcome(route, Outcome::Rejected);
                StatusCode::BAD_REQUEST.into_response()
            }
        }
    }
    .instrument(span)
    .await;
    let _ =
        nddev_device_sync_telemetry::finish_http_server_span(&finished, response.status().as_u16());
    response
}

async fn serve(
    State(app): State<Arc<App>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let started = Instant::now();
    let route = app.cache.template(uri.path());
    let span = nddev_device_sync_telemetry::http_server_span(method.as_str(), route)
        .unwrap_or_else(|_| tracing::Span::none());
    let finished = span.clone();
    let response = async move {
        let duration_ms = || started.elapsed().as_secs_f64() * 1000.0;
        let counters = app.cache.counters();
        if method != Method::GET && method != Method::HEAD {
            tracing::info!(module = "cache", event.name = "cache.rejected", method = method.as_str(), route, status = 405, duration_ms = duration_ms(), error.type = "method_not_allowed", outcome = "rejected");
            counters.outcome(route, Outcome::Rejected);
            let mut response = StatusCode::METHOD_NOT_ALLOWED.into_response();
            response.headers_mut().insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
            return response;
        }
        let accept = headers
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok());
        let base = headers
            .get(header::HOST)
            .and_then(|host| host.to_str().ok())
            .and_then(domain::loopback_base);
        let user_agent = headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok());
        match app.cache.serve(uri.path(), accept, base.as_deref(), user_agent).await {
            Ok(delivery) => {
                let served = match &delivery {
                    Delivery::Stored(object) => {
                        if object.eviction.objects > 0 {
                            tracing::info!(module = "cache", scope = "io", event.name = "cache.store.evicted", outcome = "ok");
                        }
                        if let Some(error) = object.upstream_error {
                            tracing::warn!(module = "cache", event.name = "cache.upstream.failed", route, error.type = %error, outcome = "error");
                        }
                        object.served
                    }
                    Delivery::Filling(filling) => filling.served,
                    Delivery::Answered => Served::Hit,
                };
                let head = method == Method::HEAD;
                // Stored and filling bodies stream; count what reaches the connection.
                let streams = !head && !matches!(delivery, Delivery::Answered);
                let validator = headers
                    .get(header::IF_NONE_MATCH)
                    .and_then(|value| value.to_str().ok());
                // OCI clients read the digest of what they receive from the registry's header.
                let content_digest = app.cache.oci(uri.path()).map(|path_digest| match &delivery {
                    Delivery::Stored(object) => Some(object.meta.sha256.clone()),
                    Delivery::Filling(_) | Delivery::Answered => path_digest,
                });
                let responded = match delivery {
                    Delivery::Stored(object) => respond(object, head, validator).await,
                    Delivery::Filling(filling) => respond_filling(filling, head).await,
                    Delivery::Answered => Ok(answered()),
                }
                .map(|mut response| {
                    if let Some(digest) = content_digest {
                        let headers = response.headers_mut();
                        headers.insert(DISTRIBUTION_API, HeaderValue::from_static("registry/2.0"));
                        if let Some(value) = digest.and_then(|digest| HeaderValue::from_str(&format!("sha256:{digest}")).ok()) {
                            headers.insert("docker-content-digest", value);
                        }
                    }
                    response
                });
                match responded {
                    Ok(response) => {
                        let status = response.status().as_u16();
                        tracing::info!(module = "cache", event.name = event_name(served), method = method.as_str(), route, status, duration_ms = duration_ms(), outcome = "ok");
                        counters.outcome(route, served.into());
                        if streams && response.status() != StatusCode::NOT_MODIFIED {
                            let counters = counters.clone();
                            response.map(|body| {
                                Body::from_stream(body.into_data_stream().inspect(move |chunk| {
                                    if let Ok(chunk) = chunk {
                                        counters.sent(route, chunk.len() as u64);
                                    }
                                }))
                            })
                        } else {
                            response
                        }
                    }
                    Err(_) => {
                        tracing::error!(module = "cache", scope = "io", event.name = "cache.store.read.failed", method = method.as_str(), route, status = 503, duration_ms = duration_ms(), error.type = "store_io", outcome = "error");
                        counters.outcome(route, Outcome::Failed);
                        StatusCode::SERVICE_UNAVAILABLE.into_response()
                    }
                }
            }
            Err(error) => {
                let status = error.status();
                let outcome = if matches!(error, ServeError::Rejected(_) | ServeError::NotFound) { "rejected" } else { "error" };
                counters.outcome(route, if outcome == "rejected" { Outcome::Rejected } else { Outcome::Failed });
                if outcome == "rejected" {
                    tracing::info!(module = "cache", event.name = "cache.rejected", method = method.as_str(), route, status, duration_ms = duration_ms(), error.type = error.class(), outcome);
                } else {
                    tracing::warn!(module = "cache", event.name = "cache.failed", method = method.as_str(), route, status, duration_ms = duration_ms(), error.type = error.class(), outcome);
                }
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY).into_response()
            }
        }
    }
    .instrument(span)
    .await;
    let _ =
        nddev_device_sync_telemetry::finish_http_server_span(&finished, response.status().as_u16());
    response
}

fn event_name(served: Served) -> &'static str {
    match served {
        Served::Hit => "cache.hit",
        Served::Miss => "cache.miss",
        Served::Joined => "cache.joined",
        Served::Revalidated => "cache.revalidated",
        Served::Stale => "cache.stale",
    }
}

fn content_type(value: Option<&str>) -> HeaderValue {
    value
        .and_then(|value| HeaderValue::from_str(value).ok())
        .unwrap_or(HeaderValue::from_static("application/octet-stream"))
}

/// Streams a download that is still in progress. `HEAD` answers from the upstream headers
/// without waiting for the body. A pending GET must not declare a length: even after every
/// upstream byte is flushed, verification/commit can still fail. Its framing completes only
/// when follow reaches Committed; a producer failure therefore aborts even an empty body.
async fn respond_filling(filling: Filling, head: bool) -> std::io::Result<Response> {
    let body = if head {
        Body::empty()
    } else {
        Body::from_stream(store::follow(&filling.temp, filling.progress).await?)
    };
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    if head && let Some(length) = filling.length {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    }
    headers.insert(
        header::CONTENT_TYPE,
        content_type(filling.content_type.as_deref()),
    );
    headers.insert(
        "x-nds-cache",
        HeaderValue::from_static(filling.served.as_str()),
    );
    Ok(response)
}

/// Serves a stored object. A client that already holds it, by the object's entity tag, gets
/// `304 Not Modified`; sparse registry clients such as Cargo revalidate this way. An index
/// document streams with its links rewritten, so its length is not declared.
// An empty success the cache gives itself, such as the checksum database's `/supported`.
fn answered() -> Response {
    let mut response = StatusCode::OK.into_response();
    response.headers_mut().insert(
        "x-nds-cache",
        HeaderValue::from_static(Served::Hit.as_str()),
    );
    response
}

async fn respond(object: Object, head: bool, validator: Option<&str>) -> std::io::Result<Response> {
    let vary = object.links.as_ref().map(|links| links.vary());
    let etag = match &object.links {
        Some(links) => links.tag(&object.meta.sha256),
        None => format!("\"sha256-{}\"", object.meta.sha256),
    };
    if validator.is_some_and(|validator| {
        validator
            .split(',')
            .any(|candidate| candidate.trim() == etag || candidate.trim() == "*")
    }) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        let headers = response.headers_mut();
        if let Ok(etag) = HeaderValue::from_str(&etag) {
            headers.insert(header::ETAG, etag);
        }
        if let Some(vary) = vary {
            headers.insert(header::VARY, HeaderValue::from_static(vary));
        }
        headers.insert(
            "x-nds-cache",
            HeaderValue::from_static(object.served.as_str()),
        );
        return Ok(response);
    }
    let body = if head {
        Body::empty()
    } else {
        let file = tokio::fs::File::open(&object.path).await?;
        match object.links {
            Some(links) => Body::from_stream(rewritten(file, *links)),
            None => Body::from_stream(ReaderStream::new(file)),
        }
    };
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    match vary {
        Some(vary) => {
            headers.insert(header::VARY, HeaderValue::from_static(vary));
        }
        None => {
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(object.meta.size));
        }
    }
    headers.insert(
        header::CONTENT_TYPE,
        content_type(object.meta.content_type.as_deref()),
    );
    if let Ok(etag) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, etag);
    }
    headers.insert(
        "x-nds-cache",
        HeaderValue::from_static(object.served.as_str()),
    );
    Ok(response)
}

/// The stored document with its download links rewritten chunk by chunk.
fn rewritten(
    file: tokio::fs::File,
    links: domain::Links,
) -> impl futures_util::Stream<Item = std::io::Result<bytes::Bytes>> {
    futures_util::stream::unfold(Some((ReaderStream::new(file), links)), |state| async move {
        let (mut chunks, mut links) = state?;
        match chunks.next().await {
            Some(Ok(chunk)) => Some((Ok(links.push(&chunk).into()), Some((chunks, links)))),
            Some(Err(error)) => Some((Err(error), None)),
            None => Some((Ok(links.finish().into()), None)),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Progress, Store, Validators};
    use std::sync::Mutex;
    use tokio::sync::{oneshot, watch};

    #[tokio::test]
    async fn pending_http_bodies_cannot_complete_before_native_commit() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // Real Pending files and real HTTP sockets. Control only the production progress
        // channel: all declared bytes are already flushed, but no commit is acknowledged.
        // A Content-Length regression makes each failed case complete successfully here.
        for (payload, commit) in [
            (&b""[..], false),
            (&b"already flushed"[..], false),
            (&b"verified"[..], true),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (store, _) = Store::open(&directory.path().join("store"), 1 << 30, 1 << 20)
                .await
                .unwrap();
            let mut pending = store.begin().await.unwrap();
            pending.write(payload).await.unwrap();
            pending.flush().await.unwrap();
            let path = pending.path().to_owned();
            let length = payload.len() as u64;
            let (progress, receiver) = watch::channel(Progress::Writing(length));
            let filling = move || Filling {
                served: Served::Miss,
                temp: path.clone(),
                length: Some(length),
                content_type: None,
                progress: receiver.clone(),
            };
            let head = respond_filling(filling(), true).await.unwrap();
            assert_eq!(head.headers()[header::CONTENT_LENGTH], length.to_string());
            let (accepted, accepting) = oneshot::channel();
            let accepted = Arc::new(Mutex::new(Some(accepted)));
            let router = Router::new().route(
                "/",
                get(move || {
                    let filling = filling();
                    let accepted = accepted.clone();
                    async move {
                        let response = respond_filling(filling, false).await.unwrap();
                        if let Some(signal) = accepted.lock().unwrap().take() {
                            let _ = signal.send(());
                        }
                        response
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (stop, stopping) = oneshot::channel();
            let server = tokio::spawn(async move {
                axum::serve(listener, router)
                    .with_graceful_shutdown(async {
                        let _ = stopping.await;
                    })
                    .await
                    .unwrap();
            });
            let mut client = tokio::spawn(async move {
                reqwest::Client::new()
                    .get(format!("http://{address}/"))
                    .send()
                    .await?
                    .bytes()
                    .await
            });
            tokio::time::timeout(std::time::Duration::from_secs(3), accepting)
                .await
                .unwrap()
                .unwrap();
            let premature =
                tokio::time::timeout(std::time::Duration::from_millis(100), &mut client).await;
            let incomplete = premature.is_err();
            // Settle every owned task/file before assertions, including mutation failures.
            if commit {
                let (meta, path, _) = store
                    .commit("qualification", pending, Validators::default())
                    .await
                    .unwrap();
                progress.send_replace(Progress::Committed {
                    size: meta.size,
                    path,
                });
            } else {
                drop(pending);
                progress.send_replace(Progress::Failed);
            }
            let result = match premature {
                Ok(result) => result.unwrap(),
                Err(_) => tokio::time::timeout(std::time::Duration::from_secs(3), client)
                    .await
                    .unwrap()
                    .unwrap(),
            };
            let _ = stop.send(());
            tokio::time::timeout(std::time::Duration::from_secs(3), server)
                .await
                .unwrap()
                .unwrap();
            assert!(
                incomplete,
                "HTTP completed while the complete file was still unverified"
            );
            if commit {
                assert_eq!(result.unwrap().as_ref(), payload);
            } else {
                assert!(
                    result.is_err(),
                    "failed native progress produced a complete HTTP response"
                );
            }
        }
    }
}
