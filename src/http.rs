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
async fn crates_config(method: Method, headers: HeaderMap) -> Response {
    let started = Instant::now();
    let route = "/crates-index/config.json";
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
                ([(header::CONTENT_TYPE, "application/json")], config).into_response()
            }
            None => {
                tracing::info!(module = "cache", event.name = "cache.rejected", method = method.as_str(), route, status = 400, duration_ms, error.type = "host_not_loopback", outcome = "rejected");
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
        if method != Method::GET && method != Method::HEAD {
            tracing::info!(module = "cache", event.name = "cache.rejected", method = method.as_str(), route, status = 405, duration_ms = duration_ms(), error.type = "method_not_allowed", outcome = "rejected");
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
        match app.cache.serve(uri.path(), accept, base.as_deref()).await {
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
                };
                let head = method == Method::HEAD;
                let validator = headers
                    .get(header::IF_NONE_MATCH)
                    .and_then(|value| value.to_str().ok());
                // OCI clients read the digest of what they receive from the registry's header.
                let content_digest = app.cache.oci(uri.path()).map(|path_digest| match &delivery {
                    Delivery::Stored(object) => Some(object.meta.sha256.clone()),
                    Delivery::Filling(_) => path_digest,
                });
                let responded = match delivery {
                    Delivery::Stored(object) => respond(object, head, validator).await,
                    Delivery::Filling(filling) => respond_filling(filling, head).await,
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
                        response
                    }
                    Err(_) => {
                        tracing::error!(module = "cache", scope = "io", event.name = "cache.store.read.failed", method = method.as_str(), route, status = 503, duration_ms = duration_ms(), error.type = "store_io", outcome = "error");
                        StatusCode::SERVICE_UNAVAILABLE.into_response()
                    }
                }
            }
            Err(error) => {
                let status = error.status();
                let outcome = if matches!(error, ServeError::Rejected(_) | ServeError::NotFound) { "rejected" } else { "error" };
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
/// without waiting for the body. The body carries `Content-Length` when upstream declared
/// it; a failed download ends the response early, so clients never accept a partial object.
async fn respond_filling(filling: Filling, head: bool) -> std::io::Result<Response> {
    let body = if head {
        Body::empty()
    } else {
        Body::from_stream(store::follow(&filling.temp, filling.progress).await?)
    };
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    if let Some(length) = filling.length {
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
async fn respond(object: Object, head: bool, validator: Option<&str>) -> std::io::Result<Response> {
    let index = object.links.is_some();
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
        if index {
            headers.insert(header::VARY, HeaderValue::from_static("accept"));
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
            Some(links) => Body::from_stream(rewritten(file, links)),
            None => Body::from_stream(ReaderStream::new(file)),
        }
    };
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    if index {
        headers.insert(header::VARY, HeaderValue::from_static("accept"));
    } else {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(object.meta.size));
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
