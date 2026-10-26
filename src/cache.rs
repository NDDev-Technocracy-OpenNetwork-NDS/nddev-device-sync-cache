//! The serve use case: store lookup, single-flight upstream fetch, background fills that
//! clients follow while they are written, revalidation of mutable objects and stale service
//! when revalidation fails.
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tracing::Instrument;

use crate::{
    domain::{Freshness, Links, Rejection, Routes, Target},
    store::{Eviction, Meta, Pending, Progress, Store, StoreError, Validators, now},
    upstream::{Body, Fetched, Upstream, UpstreamError},
};

/// How a response was produced, for telemetry and the `x-nds-cache` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Served {
    Hit,
    Miss,
    /// Served from a download that another request started.
    Joined,
    Revalidated,
    Stale,
}

impl Served {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Joined => "joined",
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

/// A complete stored object.
pub struct Object {
    pub path: PathBuf,
    pub meta: Meta,
    pub served: Served,
    pub eviction: Eviction,
    pub upstream_error: Option<UpstreamError>,
    /// Set for an index document, whose download links are rewritten as it is served.
    pub links: Option<Links>,
}

/// An immutable object that is still downloading; clients follow its progress.
pub struct Filling {
    pub served: Served,
    pub temp: PathBuf,
    pub length: Option<u64>,
    pub content_type: Option<String>,
    pub progress: watch::Receiver<Progress>,
}

pub enum Delivery {
    Stored(Object),
    Filling(Filling),
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
            Self::Rejected(rejection) => rejection.status(),
            Self::NotFound => 404,
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

/// One background download registered for its store key.
struct Fill {
    temp: PathBuf,
    length: Option<u64>,
    content_type: Option<String>,
    progress: watch::Receiver<Progress>,
}

impl Fill {
    fn follow(&self, served: Served) -> Filling {
        Filling {
            served,
            temp: self.temp.clone(),
            length: self.length,
            content_type: self.content_type.clone(),
            progress: self.progress.clone(),
        }
    }
}

type Fills = Arc<Mutex<HashMap<String, Arc<Fill>>>>;

pub struct Cache {
    routes: Arc<Routes>,
    store: Arc<Store>,
    upstream: Upstream,
    flights: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    fills: Fills,
    fetches: Arc<Semaphore>,
    fetch_wait: Duration,
    revalidate_after: u64,
    upstream_state: Arc<AtomicU8>,
}

fn stored_validators(meta: &Meta) -> Validators {
    Validators {
        content_type: meta.content_type.clone(),
        etag: meta.etag.clone(),
        last_modified: meta.last_modified.clone(),
    }
}

/// Records the upstream outcome and reports a change of the dependency state.
fn record(state: &AtomicU8, next: UpstreamState) {
    let previous = state.swap(next as u8, Ordering::Relaxed);
    if previous == next as u8 {
        return;
    }
    match next {
        UpstreamState::Ok => tracing::info!(
            module = "cache",
            scope = "transport",
            event.name = "cache.upstream.available",
            available = true,
            outcome = "ready"
        ),
        UpstreamState::Failing => tracing::warn!(
            module = "cache",
            scope = "transport",
            event.name = "cache.upstream.degraded",
            available = false,
            error.type = "upstream_failing",
            outcome = "unavailable"
        ),
        UpstreamState::Unknown => {}
    }
}

fn stored(meta: Meta, path: PathBuf, served: Served) -> Object {
    Object {
        path,
        meta,
        served,
        eviction: Eviction::default(),
        upstream_error: None,
        links: None,
    }
}

impl Cache {
    pub fn new(routes: Arc<Routes>, store: Store, upstream: Upstream, limits: Limits) -> Self {
        Self {
            routes,
            store: Arc::new(store),
            upstream,
            flights: Mutex::new(HashMap::new()),
            fills: Arc::default(),
            fetches: Arc::new(Semaphore::new(limits.fetches)),
            fetch_wait: limits.fetch_wait,
            revalidate_after: limits.revalidate_after.as_secs(),
            upstream_state: Arc::new(AtomicU8::new(UpstreamState::Unknown as u8)),
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

    /// The bounded telemetry route of a request path: its matched template or `unmatched`.
    pub fn template(&self, path: &str) -> &'static str {
        self.routes
            .resolve(path, None)
            .map(|target| target.route.template)
            .unwrap_or("unmatched")
    }

    fn fresh(&self, target: &Target<'_>, meta: &Meta) -> bool {
        target.freshness == Freshness::Immutable
            || now().saturating_sub(meta.fetched_at) < self.revalidate_after
    }

    fn joined(&self, key: &str) -> Option<Filling> {
        let fills = self.fills.lock().ok()?;
        fills.get(key).map(|fill| fill.follow(Served::Joined))
    }

    /// Serves `path` in the representation `accept` selects. `base` is the loopback base address
    /// the client used, if any; an index document needs it to point its links back here.
    pub async fn serve(
        &self,
        path: &str,
        accept: Option<&str>,
        base: Option<&str>,
    ) -> Result<Delivery, ServeError> {
        let target = self
            .routes
            .resolve(path, accept)
            .map_err(ServeError::Rejected)?;
        let links = match target.index() {
            Some(index) => Some(Links::new(
                index,
                base.ok_or(ServeError::Rejected(Rejection::HostNotLoopback))?,
            )),
            None => None,
        };
        let key = target.key();
        if let Some((meta, path)) = self
            .store
            .get(&key)
            .filter(|(meta, _)| self.fresh(&target, meta))
        {
            return Ok(Delivery::Stored(Object {
                links,
                ..stored(meta, path, Served::Hit)
            }));
        }
        if let Some(filling) = self.joined(&key) {
            return Ok(Delivery::Filling(filling));
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
            match target.freshness {
                Freshness::Immutable => self.start(&target, &key).await,
                Freshness::Revalidate => self
                    .fill(&target, &key)
                    .await
                    .map(|object| Delivery::Stored(Object { links, ..object })),
            }
        };
        if let Ok(mut flights) = self.flights.lock()
            && Arc::strong_count(&flight) == 2
        {
            flights.remove(&key);
        }
        result
    }

    async fn permit(&self) -> Result<OwnedSemaphorePermit, ServeError> {
        tokio::time::timeout(self.fetch_wait, self.fetches.clone().acquire_owned())
            .await
            .map_err(|_| ServeError::Busy)?
            .map_err(|_| ServeError::Busy)
    }

    async fn fetch(
        &self,
        target: &Target<'_>,
        stored: Option<&Validators>,
    ) -> Result<Fetched, ServeError> {
        let fetched = self
            .upstream
            .fetch(&target.upstream_url(), target.media_type, stored)
            .await;
        record(
            &self.upstream_state,
            if fetched.is_ok() {
                UpstreamState::Ok
            } else {
                UpstreamState::Failing
            },
        );
        fetched.map_err(ServeError::Upstream)
    }

    /// Runs with the key's flight held: starts one background download of an immutable
    /// object and returns it for following, or serves what an earlier flight produced.
    async fn start(&self, target: &Target<'_>, key: &str) -> Result<Delivery, ServeError> {
        if let Some((meta, path)) = self.store.get(key) {
            return Ok(Delivery::Stored(stored(meta, path, Served::Hit)));
        }
        if let Some(filling) = self.joined(key) {
            return Ok(Delivery::Filling(filling));
        }
        let permit = self.permit().await?;
        let (validators, length, body) = match self.fetch(target, None).await? {
            Fetched::Content(validators, length, body) => (validators, length, body),
            Fetched::NotFound => return Err(ServeError::NotFound),
            Fetched::NotModified(_) => return Err(ServeError::Upstream(UpstreamError::Status)),
        };
        if length.is_some_and(|length| length > self.store.max_object()) {
            return Err(ServeError::Store(StoreError::TooLarge));
        }
        let pending = self.store.begin().await.map_err(ServeError::Store)?;
        let (progress, follower) = watch::channel(Progress::Writing(0));
        let fill = Arc::new(Fill {
            temp: pending.path().to_owned(),
            length,
            content_type: validators.content_type.clone(),
            progress: follower,
        });
        self.fills
            .lock()
            .map_err(|_| ServeError::Store(StoreError::Io))?
            .insert(key.to_owned(), fill.clone());
        // The download outlives its request; its own operation span stays in the
        // request's trace, so the fill events correlate with the request that started it.
        let span = nddev_device_sync_telemetry::operation_span("cache", "io")
            .unwrap_or_else(|_| tracing::Span::none());
        let job = Download {
            store: self.store.clone(),
            fills: self.fills.clone(),
            fill: fill.clone(),
            upstream_state: self.upstream_state.clone(),
            key: key.to_owned(),
            route: target.route.template,
            pending,
            validators,
            body,
            progress,
            started: Instant::now(),
            span: span.clone(),
            _permit: permit,
        };
        tokio::spawn(download(job).instrument(span));
        Ok(Delivery::Filling(fill.follow(Served::Miss)))
    }

    /// Runs with the key's flight held, so one upstream request serves concurrent callers.
    async fn fill(&self, target: &Target<'_>, key: &str) -> Result<Object, ServeError> {
        let stored_object = self.store.get(key);
        if let Some((meta, path)) = &stored_object
            && self.fresh(target, meta)
        {
            return Ok(stored(meta.clone(), path.clone(), Served::Hit));
        }
        let _permit = self.permit().await?;
        let conditional = stored_object
            .as_ref()
            .map(|(meta, _)| stored_validators(meta));
        match self.fetch(target, conditional.as_ref()).await {
            Ok(Fetched::NotModified(validators)) => {
                let (_, path) = stored_object.ok_or(ServeError::Upstream(UpstreamError::Status))?;
                let meta = self
                    .store
                    .refresh(key, validators)
                    .await
                    .map_err(ServeError::Store)?;
                Ok(stored(meta, path, Served::Revalidated))
            }
            Ok(Fetched::NotFound) => Err(ServeError::NotFound),
            Ok(Fetched::Content(validators, _, mut body)) => {
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
                        eviction,
                        ..stored(meta, path, Served::Miss)
                    }),
                    Err(error) => {
                        if matches!(error, ServeError::Upstream(_)) {
                            record(&self.upstream_state, UpstreamState::Failing);
                        }
                        self.stale_or(stored_object, error)
                    }
                }
            }
            Err(error) => self.stale_or(stored_object, error),
        }
    }

    /// A stored mutable object is served when its revalidation fails; the failure stays visible.
    fn stale_or(
        &self,
        stored_object: Option<(Meta, PathBuf)>,
        error: ServeError,
    ) -> Result<Object, ServeError> {
        match (stored_object, &error) {
            (Some((meta, path)), ServeError::Upstream(upstream_error)) => Ok(Object {
                upstream_error: Some(*upstream_error),
                ..stored(meta, path, Served::Stale)
            }),
            _ => Err(error),
        }
    }
}

/// Everything a background download owns; it outlives the request that started it.
struct Download {
    store: Arc<Store>,
    fills: Fills,
    fill: Arc<Fill>,
    upstream_state: Arc<AtomicU8>,
    key: String,
    route: &'static str,
    pending: Pending,
    validators: Validators,
    body: Body,
    progress: watch::Sender<Progress>,
    started: Instant,
    span: tracing::Span,
    _permit: OwnedSemaphorePermit,
}

/// Writes the body to the store, publishing progress after every flushed chunk, then
/// commits it. A failed download leaves no object and fails its followers.
async fn download(mut job: Download) {
    let written = async {
        while let Some(chunk) = job.body.next().await {
            let chunk = chunk.map_err(ServeError::Upstream)?;
            job.pending.write(&chunk).await.map_err(ServeError::Store)?;
            job.pending.flush().await.map_err(ServeError::Store)?;
            job.progress
                .send_replace(Progress::Writing(job.pending.size()));
        }
        Ok(())
    }
    .await;
    let committed = match written {
        Ok(()) => job
            .store
            .commit(&job.key, job.pending, job.validators)
            .await
            .map_err(ServeError::Store),
        Err(error) => {
            drop(job.pending);
            Err(error)
        }
    };
    let route = job.route;
    let duration_ms = job.started.elapsed().as_secs_f64() * 1000.0;
    nddev_device_sync_telemetry::finish_operation_span(&job.span, committed.is_err());
    match committed {
        Ok((meta, path, eviction)) => {
            job.progress.send_replace(Progress::Committed {
                size: meta.size,
                path,
            });
            tracing::info!(
                module = "cache",
                scope = "io",
                event.name = "cache.fill.committed",
                route,
                duration_ms,
                outcome = "ok"
            );
            if eviction.objects > 0 {
                tracing::info!(
                    module = "cache",
                    scope = "io",
                    event.name = "cache.store.evicted",
                    outcome = "ok"
                );
            }
        }
        Err(error) => {
            if matches!(error, ServeError::Upstream(_)) {
                record(&job.upstream_state, UpstreamState::Failing);
            }
            job.progress.send_replace(Progress::Failed);
            tracing::warn!(module = "cache", scope = "io", event.name = "cache.fill.failed", route, duration_ms, error.type = error.class(), outcome = "error");
        }
    }
    if let Ok(mut fills) = job.fills.lock()
        && fills
            .get(&job.key)
            .is_some_and(|current| Arc::ptr_eq(current, &job.fill))
    {
        fills.remove(&job.key);
    }
}
