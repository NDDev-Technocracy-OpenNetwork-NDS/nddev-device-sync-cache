//! Optional compiled-result store. Its credentials and objects are separate from public downloads.
use crate::store::{Collection, Store, StoreError, Validators};
use axum::{
    body::Body,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    fs,
    sync::{Mutex, RwLock, Semaphore},
};
use tokio_util::io::ReaderStream;

pub const ROUTE: &str = "/v1/build-cache/{key}";
const PREFIX: &str = "/v1/build-cache/";

#[derive(Clone, Debug)]
pub struct Settings {
    pub root: PathBuf,
    pub credentials: PathBuf,
    pub capacity: u64,
    pub max_object: u64,
    pub retention: Duration,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    schema: u32,
    readers: Vec<String>,
    ci_writers: Vec<String>,
}
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct CollectionReport {
    pub busy: bool,
    pub removed_objects: usize,
    pub removed_bytes: u64,
    pub failed_objects: usize,
}
pub struct BuildStore {
    store: Store,
    credentials: Credentials,
    retention: Duration,
    // Readers hold a shared lease for the full response body. Commit and GC take exclusion.
    gate: Arc<RwLock<()>>,
    touches: Mutex<()>,
    uploads: Semaphore,
    last_collection: std::sync::Mutex<CollectionReport>,
}

fn hex(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn credential_hash(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn key(uri: &Uri) -> Option<&str> {
    if uri.query().is_some() {
        return None;
    }
    let value = uri.path().strip_prefix(PREFIX)?;
    if value.len() > 512 {
        return None;
    }
    if value == "sccache/.sccache_check" {
        return Some(value);
    }
    let (kind, path) = value.split_once('/')?;
    if !matches!(kind, "sccache" | "gradle") {
        return None;
    }
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() > 8 || !parts.iter().all(|part| part.len() <= 128 && hex(part)) {
        return None;
    }
    // Native keys are hashes. Sharding components may be single hex digits.
    if parts.last()?.len() < 32 {
        return None;
    }
    Some(value)
}

impl BuildStore {
    pub async fn open(settings: &Settings, download_root: &Path) -> Result<Self, StoreError> {
        let download_root = fs::canonicalize(download_root).await?;
        let metadata = fs::symlink_metadata(&settings.credentials).await?;
        if !metadata.is_file() || metadata.len() > 65536 {
            return Err(StoreError::Unowned);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(StoreError::Unowned);
            }
        }
        let credentials: Credentials =
            serde_json::from_slice(&fs::read(&settings.credentials).await?)
                .map_err(|_| StoreError::Unowned)?;
        let valid = |values: &[String]| values.iter().all(|value| value.len() == 64 && hex(value));
        if credentials.schema != 1
            || credentials.ci_writers.is_empty()
            || !valid(&credentials.readers)
            || !valid(&credentials.ci_writers)
            || credentials
                .readers
                .iter()
                .any(|r| credentials.ci_writers.contains(r))
        {
            return Err(StoreError::Unowned);
        }
        // Refuse aliases of the download store before opening or cleaning anything.
        let parent = fs::canonicalize(settings.root.parent().ok_or(StoreError::Unowned)?).await?;
        let root = parent.join(settings.root.file_name().ok_or(StoreError::Unowned)?);
        if fs::symlink_metadata(&root)
            .await
            .is_ok_and(|m| m.file_type().is_symlink())
            || root.starts_with(&download_root)
            || download_root.starts_with(&root)
        {
            return Err(StoreError::Unowned);
        }
        let credentials_path = fs::canonicalize(&settings.credentials).await?;
        if credentials_path.starts_with(&root) || credentials_path.starts_with(&download_root) {
            return Err(StoreError::Unowned);
        }
        let (store, _) =
            Store::open_preserving_unknown(&root, settings.capacity, settings.max_object).await?;
        Ok(Self {
            store,
            credentials,
            retention: settings.retention,
            gate: Arc::new(RwLock::new(())),
            touches: Mutex::new(()),
            uploads: Semaphore::new(2),
            last_collection: std::sync::Mutex::default(),
        })
    }

    pub async fn ready(&self) -> bool {
        self.store.ready().await
    }
    pub fn health(&self) -> serde_json::Value {
        let usage = self.store.usage();
        serde_json::json!({"objects":usage.objects,"bytes":usage.bytes,"capacity":usage.capacity,
            "idle_retention_seconds":self.retention.as_secs(),
            "last_collection":self.last_collection.lock().ok().map(|v| *v)})
    }
    async fn touch(&self, key: &str) -> Result<(), StoreError> {
        let _touch = self.touches.lock().await;
        self.store.touch(key).await
    }
    pub async fn collect(&self, now: u64) -> Result<CollectionReport, StoreError> {
        let Ok(_exclusive) = self.gate.clone().try_write_owned() else {
            let report = CollectionReport {
                busy: true,
                ..Default::default()
            };
            *self.last_collection.lock().map_err(|_| StoreError::Io)? = report;
            return Ok(report);
        };
        let Collection {
            removed_objects,
            removed_bytes,
            failed_objects,
        } = self
            .store
            .collect_idle(now, self.retention.as_secs())
            .await?;
        let report = CollectionReport {
            busy: false,
            removed_objects,
            removed_bytes,
            failed_objects,
        };
        *self.last_collection.lock().map_err(|_| StoreError::Io)? = report;
        Ok(report)
    }
    pub async fn respond(
        self: &Arc<Self>,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        body: Body,
    ) -> Response {
        let Some(key) = key(uri) else {
            return if method == Method::GET || method == Method::HEAD {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            }
            .into_response();
        };
        if headers.contains_key(header::COOKIE)
            || headers.contains_key(header::PROXY_AUTHORIZATION)
            || headers.get_all(header::AUTHORIZATION).iter().count() != 1
        {
            return StatusCode::FORBIDDEN.into_response();
        }
        let Some(auth) = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .filter(|s| s.len() <= 512)
        else {
            return StatusCode::FORBIDDEN.into_response();
        };
        let digest = credential_hash(auth);
        let writer = self.credentials.ci_writers.contains(&digest);
        let reader = writer || self.credentials.readers.contains(&digest);
        if !reader || (method == Method::PUT && !writer) {
            return StatusCode::FORBIDDEN.into_response();
        }
        match *method {
            Method::GET | Method::HEAD => self.read(key, method == Method::HEAD).await,
            Method::PUT => self.write(key, headers, body).await,
            _ => (
                StatusCode::METHOD_NOT_ALLOWED,
                [(header::ALLOW, "GET, HEAD, PUT")],
            )
                .into_response(),
        }
    }
    async fn read(self: &Arc<Self>, key: &str, head: bool) -> Response {
        let guard = self.gate.clone().read_owned().await;
        let Some((meta, path)) = self.store.get(key) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if self.touch(key).await.is_err() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let Ok(file) = fs::File::open(path).await else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let body = if head {
            Body::empty()
        } else {
            let state = (ReaderStream::new(file), guard, self.clone(), key.to_owned());
            Body::from_stream(futures_util::stream::unfold(
                state,
                |(mut stream, guard, store, key)| async move {
                    match stream.next().await {
                        Some(chunk) => Some((chunk, (stream, guard, store, key))),
                        None => {
                            let _ = store.touch(&key).await;
                            None
                        }
                    }
                },
            ))
        };
        (
            [
                (header::CONTENT_LENGTH, meta.size.to_string()),
                (header::CONTENT_TYPE, "application/octet-stream".into()),
                (header::ETAG, format!("\"{}\"", meta.sha256)),
                (header::CACHE_CONTROL, "private, no-store".into()),
            ],
            body,
        )
            .into_response()
    }
    async fn write(&self, key: &str, headers: &HeaderMap, body: Body) -> Response {
        let Some(length) = headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
        else {
            return StatusCode::LENGTH_REQUIRED.into_response();
        };
        if length > self.store.max_object() {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        let Ok(_permit) = self.uploads.try_acquire() else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        if !self.store.ready().await {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let Ok(mut pending) = self.store.begin().await else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let mut chunks = body.into_data_stream();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(60), chunks.next()).await;
            match next {
                Ok(Some(Ok(bytes))) => match pending.write(&bytes).await {
                    Ok(()) => {}
                    Err(StoreError::TooLarge) => {
                        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
                    }
                    Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
                },
                Ok(None) => break,
                Ok(Some(Err(_))) => return StatusCode::BAD_REQUEST.into_response(),
                Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
            }
        }
        if pending.size() != length {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let Ok(_exclusive) = tokio::time::timeout(Duration::from_secs(30), self.gate.write()).await
        else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        if let Some((meta, _)) = self.store.get(key) {
            if meta.sha256 != pending.sha256() {
                return StatusCode::CONFLICT.into_response();
            }
            return if self.touch(key).await.is_ok() {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            }
            .into_response();
        }
        match self.store.commit(key, pending, Validators::default()).await {
            Ok(_) => StatusCode::CREATED.into_response(),
            Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    }
}
