//! HTTP adapter: protocol routes, health, readiness and the AGPL source offer.
use std::{sync::Arc, time::Instant};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use tokio_util::io::ReaderStream;
use tracing::Instrument;

use crate::{
    REPOSITORY, SOURCE_COMMIT,
    cache::{Cache, Object, ServeError},
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

async fn serve(State(app): State<Arc<App>>, method: Method, uri: Uri) -> Response {
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
        match app.cache.serve(uri.path()).await {
            Ok(object) => {
                if object.eviction.objects > 0 {
                    tracing::info!(module = "cache", scope = "io", event.name = "cache.store.evicted", outcome = "ok");
                }
                if let Some(error) = object.upstream_error {
                    tracing::warn!(module = "cache", event.name = "cache.upstream.failed", route, error.type = %error, outcome = "error");
                }
                let served = object.served;
                match respond(object, method == Method::HEAD).await {
                    Ok(response) => {
                        tracing::info!(module = "cache", event.name = event_name(served), method = method.as_str(), route, status = 200, duration_ms = duration_ms(), outcome = "ok");
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

fn event_name(served: crate::cache::Served) -> &'static str {
    match served {
        crate::cache::Served::Hit => "cache.hit",
        crate::cache::Served::Miss => "cache.miss",
        crate::cache::Served::Revalidated => "cache.revalidated",
        crate::cache::Served::Stale => "cache.stale",
    }
}

async fn respond(object: Object, head: bool) -> std::io::Result<Response> {
    let content_type = object
        .meta
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    let body = if head {
        Body::empty()
    } else {
        Body::from_stream(ReaderStream::new(
            tokio::fs::File::open(&object.path).await?,
        ))
    };
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(object.meta.size));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    if let Ok(etag) = HeaderValue::from_str(&format!("\"sha256-{}\"", object.meta.sha256)) {
        headers.insert(header::ETAG, etag);
    }
    headers.insert(
        "x-nds-cache",
        HeaderValue::from_static(object.served.as_str()),
    );
    Ok(response)
}
