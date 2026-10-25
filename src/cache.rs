//! The serve use case: store lookup, single-flight upstream fetch, revalidation of mutable
//! objects and stale service when revalidation fails.
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use tokio::sync::Semaphore;

use crate::{
    domain::{Freshness, Rejection, Routes, Target},
    store::{Eviction, Meta, Store, StoreError, Validators, now},
    upstream::{Fetched, Upstream, UpstreamError},
};

/// How a response was produced, for telemetry and the `x-nds-cache` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Served {
    Hit,
    Miss,
    Revalidated,
    Stale,
}

impl Served {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Revalidated => "revalidated",
            Self::Stale => "stale",
        }
    }
}

/// Outcome of the latest upstream exchange. Readiness reports it without failing on it,
/// because stored objects stay servable while upstream is unreachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamState {
    Unknown,
    Ok,
    Failing,
}

impl UpstreamState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Ok => "ok",
            Self::Failing => "failing",
        }
    }
}

pub struct Object {
    pub path: PathBuf,
    pub meta: Meta,
    pub served: Served,
    pub eviction: Eviction,
    pub upstream_error: Option<UpstreamError>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ServeError {
    #[error("request_rejected")]
    Rejected(Rejection),
    #[error("upstream_not_found")]
    NotFound,
    #[error("fetch_capacity")]
    Busy,
    #[error("upstream_failed")]
    Upstream(UpstreamError),
    #[error("store_failed")]
    Store(StoreError),
}

impl ServeError {
    pub fn class(&self) -> &'static str {
        match self {
            Self::Rejected(rejection) => rejection.class(),
            Self::NotFound => "upstream_not_found",
            Self::Busy => "fetch_capacity",
            Self::Upstream(UpstreamError::Connect) => "upstream_connect",
            Self::Upstream(UpstreamError::Timeout) => "upstream_timeout",
            Self::Upstream(UpstreamError::Redirect) => "upstream_redirect",
            Self::Upstream(UpstreamError::Status) => "upstream_status",
            Self::Upstream(UpstreamError::Body) => "upstream_body",
            Self::Store(StoreError::TooLarge) => "object_too_large",
            Self::Store(StoreError::Unowned) => "store_unowned",
            Self::Store(StoreError::Io) => "store_io",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::Rejected(_) | Self::NotFound => 404,
            Self::Busy => 503,
            Self::Store(StoreError::TooLarge) => 502,
            Self::Upstream(UpstreamError::Timeout) => 504,
            Self::Upstream(_) => 502,
            Self::Store(_) => 503,
        }
    }
}

pub struct Limits {
    pub fetches: usize,
    pub fetch_wait: Duration,
    pub revalidate_after: Duration,
}

pub struct Cache {
    routes: Arc<Routes>,
    store: Store,
    upstream: Upstream,
    flights: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    fetches: Semaphore,
    fetch_wait: Duration,
    revalidate_after: u64,
    upstream_state: AtomicU8,
}

fn stored_validators(meta: &Meta) -> Validators {
    Validators {
        content_type: meta.content_type.clone(),
        etag: meta.etag.clone(),
        last_modified: meta.last_modified.clone(),
    }
}

impl Cache {
    pub fn new(routes: Arc<Routes>, store: Store, upstream: Upstream, limits: Limits) -> Self {
        Self {
            routes,
            store,
            upstream,
            flights: Mutex::new(HashMap::new()),
            fetches: Semaphore::new(limits.fetches),
            fetch_wait: limits.fetch_wait,
            revalidate_after: limits.revalidate_after.as_secs(),
            upstream_state: AtomicU8::new(UpstreamState::Unknown as u8),
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn upstream_state(&self) -> UpstreamState {
        match self.upstream_state.load(Ordering::Relaxed) {
            state if state == UpstreamState::Ok as u8 => UpstreamState::Ok,
            state if state == UpstreamState::Failing as u8 => UpstreamState::Failing,
            _ => UpstreamState::Unknown,
        }
    }

    fn record(&self, state: UpstreamState) {
        self.upstream_state.store(state as u8, Ordering::Relaxed);
    }

    /// The bounded telemetry route of a request path: its matched template or `unmatched`.
    pub fn template(&self, path: &str) -> &'static str {
        self.routes
            .resolve(path)
            .map(|target| target.route.template)
            .unwrap_or("unmatched")
    }

    fn fresh(&self, target: &Target<'_>, meta: &Meta) -> bool {
        target.freshness == Freshness::Immutable
            || now().saturating_sub(meta.fetched_at) < self.revalidate_after
    }

    pub async fn serve(&self, path: &str) -> Result<Object, ServeError> {
        let target = self.routes.resolve(path).map_err(ServeError::Rejected)?;
        let key = target.key();
        if let Some((meta, path)) = self
            .store
            .get(&key)
            .filter(|(meta, _)| self.fresh(&target, meta))
        {
            return Ok(Object {
                path,
                meta,
                served: Served::Hit,
                eviction: Eviction::default(),
                upstream_error: None,
            });
        }
        let flight = {
            let mut flights = self
                .flights
                .lock()
                .map_err(|_| ServeError::Store(StoreError::Io))?;
            flights.entry(key.clone()).or_default().clone()
        };
        let result = {
            let _guard = flight.lock().await;
            self.fill(&target, &key).await
        };
        if let Ok(mut flights) = self.flights.lock()
            && Arc::strong_count(&flight) == 2
        {
            flights.remove(&key);
        }
        result
    }

    /// Runs with the key's flight held, so one upstream request serves concurrent callers.
    async fn fill(&self, target: &Target<'_>, key: &str) -> Result<Object, ServeError> {
        let stored = self.store.get(key);
        if let Some((meta, path)) = &stored
            && self.fresh(target, meta)
        {
            return Ok(Object {
                path: path.clone(),
                meta: meta.clone(),
                served: Served::Hit,
                eviction: Eviction::default(),
                upstream_error: None,
            });
        }
        let _permit = tokio::time::timeout(self.fetch_wait, self.fetches.acquire())
            .await
            .map_err(|_| ServeError::Busy)?
            .map_err(|_| ServeError::Busy)?;
        let conditional = stored.as_ref().map(|(meta, _)| stored_validators(meta));
        let fetched = self
            .upstream
            .fetch(&target.upstream_url(), conditional.as_ref())
            .await;
        self.record(if fetched.is_ok() {
            UpstreamState::Ok
        } else {
            UpstreamState::Failing
        });
        match fetched {
            Ok(Fetched::NotModified(validators)) => {
                let (_, path) = stored.ok_or(ServeError::Upstream(UpstreamError::Status))?;
                let meta = self
                    .store
                    .refresh(key, validators)
                    .await
                    .map_err(ServeError::Store)?;
                Ok(Object {
                    path,
                    meta,
                    served: Served::Revalidated,
                    eviction: Eviction::default(),
                    upstream_error: None,
                })
            }
            Ok(Fetched::NotFound) => Err(ServeError::NotFound),
            Ok(Fetched::Content(validators, mut body)) => {
                let written = async {
                    let mut pending = self.store.begin().await.map_err(ServeError::Store)?;
                    while let Some(chunk) = body.next().await {
                        let chunk = chunk.map_err(ServeError::Upstream)?;
                        pending.write(&chunk).await.map_err(ServeError::Store)?;
                    }
                    self.store
                        .commit(key, pending, validators)
                        .await
                        .map_err(ServeError::Store)
                }
                .await;
                match written {
                    Ok((meta, path, eviction)) => Ok(Object {
                        path,
                        meta,
                        served: Served::Miss,
                        eviction,
                        upstream_error: None,
                    }),
                    Err(error) => {
                        if matches!(error, ServeError::Upstream(_)) {
                            self.record(UpstreamState::Failing);
                        }
                        self.stale_or(stored, error)
                    }
                }
            }
            Err(error) => self.stale_or(stored, ServeError::Upstream(error)),
        }
    }

    /// A stored mutable object is served when its revalidation fails; the failure stays visible.
    fn stale_or(
        &self,
        stored: Option<(Meta, PathBuf)>,
        error: ServeError,
    ) -> Result<Object, ServeError> {
        match (stored, &error) {
            (Some((meta, path)), ServeError::Upstream(upstream_error)) => Ok(Object {
                path,
                meta,
                served: Served::Stale,
                eviction: Eviction::default(),
                upstream_error: Some(*upstream_error),
            }),
            _ => Err(error),
        }
    }
}
