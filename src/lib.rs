//! NDS central network cache. Proxies an allowlist of upstream routes, keeps their objects
//! on the cache volume and reports through the shared NDS telemetry envelope.
#![forbid(unsafe_code)]

pub mod builds;
pub mod cache;
pub mod config;
pub mod counters;
pub mod domain;
pub mod http;
pub mod store;
pub mod telemetry;
pub mod upstream;

use std::{future::Future, sync::Arc, time::Duration};

use cache::{Cache, Limits};
use config::Settings;
use domain::Routes;
use store::{Store, StoreError};
use telemetry::Identity;
use upstream::{Upstream, UpstreamError};

pub const REPOSITORY: &str = "NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync-cache";
pub const SOURCE_COMMIT: &str = match option_env!("NDS_BUILD_COMMIT") {
    Some(commit) => commit,
    None => "unknown",
};

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("store_failed")]
    Store(#[from] StoreError),
    #[error("upstream_client")]
    Upstream(#[from] UpstreamError),
    #[error("transport_listener")]
    Listener,
}

impl RunError {
    pub fn class(&self) -> &'static str {
        match self {
            Self::Store(StoreError::Unowned) => "store_unowned",
            Self::Store(_) => "store_io",
            Self::Upstream(_) => "upstream_client",
            Self::Listener => "transport_listener",
        }
    }
}

/// Idle collection of public downloads. Before this feature, hit timestamps were memory-only;
/// a fresh process first observes a full retention window instead of inventing prior last use.
pub struct DownloadGc {
    pub gate: Arc<tokio::sync::RwLock<()>>,
    retention: u64,
    opened_at: u64,
    last: std::sync::Mutex<serde_json::Value>,
}
impl DownloadGc {
    fn new(retention: Duration) -> Self {
        Self {
            gate: Arc::new(tokio::sync::RwLock::new(())),
            retention: retention.as_secs(),
            opened_at: store::now(),
            last: std::sync::Mutex::new(serde_json::Value::Null),
        }
    }
    pub fn health(&self) -> serde_json::Value {
        serde_json::json!({"idle_retention_seconds":self.retention,
            "protection_grace_until":self.opened_at.saturating_add(self.retention),
            "last_collection":self.last.lock().ok().map(|r|r.clone())})
    }
    pub async fn collect(&self, cache: &Cache, time: u64) -> Result<serde_json::Value, StoreError> {
        let mut report = serde_json::json!({"busy":false,"protection_grace":false,
            "removed_objects":0,"removed_bytes":0,"failed_objects":0});
        if !time
            .checked_sub(self.opened_at)
            .is_some_and(|age| age >= self.retention)
        {
            report["protection_grace"] = true.into();
        } else if let Ok(_exclusive) = self.gate.clone().try_write_owned() {
            if cache.has_active_fills() {
                report["busy"] = true.into();
            } else {
                let result = cache.store().collect_idle(time, self.retention).await?;
                report["removed_objects"] = result.removed_objects.into();
                report["removed_bytes"] = result.removed_bytes.into();
                report["failed_objects"] = result.failed_objects.into();
            }
        } else {
            report["busy"] = true.into();
        }
        *self.last.lock().map_err(|_| StoreError::Io)? = report.clone();
        Ok(report)
    }
}

/// Opens the store and builds the application with an explicit route table.
pub async fn app(
    settings: &Settings,
    identity: Identity,
    routes: Routes,
) -> Result<Arc<http::App>, RunError> {
    let routes = Arc::new(routes);
    let (store, invalid) =
        Store::open(&settings.root, settings.capacity, settings.max_object).await?;
    tracing::info!(
        module = "cache",
        scope = "io",
        event.name = "cache.store.opened",
        available = true,
        rejected_count = invalid as u64,
        outcome = "ready"
    );
    let user_agent = format!("nddev-device-sync-cache/{}", identity.version);
    let upstream = Upstream::new(
        routes.clone(),
        &user_agent,
        settings.connect_timeout,
        settings.read_timeout,
    )?;
    let cache = Cache::new(
        routes,
        store,
        upstream,
        Limits {
            fetches: settings.fetches,
            fetch_wait: settings.fetch_wait,
            revalidate_after: settings.revalidate_after,
        },
    );
    let builds = match &settings.builds {
        Some(builds) => Some(Arc::new(
            crate::builds::BuildStore::open(builds, &settings.root).await?,
        )),
        None => None,
    };
    Ok(Arc::new(http::App {
        cache,
        identity,
        builds,
        downloads_gc: settings.idle_retention.map(DownloadGc::new),
    }))
}

/// Serves until `shutdown` resolves, then drains connections within the grace period.
pub async fn serve(
    settings: &Settings,
    app: Arc<http::App>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), RunError> {
    let listener = tokio::net::TcpListener::bind(settings.listen)
        .await
        .map_err(|_| RunError::Listener)?;
    tracing::info!(
        module = "cache",
        scope = "transport",
        event.name = "transport.listening",
        transport = "http",
        outcome = "ready"
    );
    let watcher = tokio::spawn(watch_store(app.clone()));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, http::router(app)).with_graceful_shutdown(async move {
        shutdown.await;
        let _ = stop.send(());
    });
    let grace = settings.shutdown_grace;
    let result = tokio::select! {
        result = server => result.map_err(|_| RunError::Listener),
        _ = async move {
            let _ = stopped.await;
            tokio::time::sleep(grace).await;
        } => Ok(()),
    };
    watcher.abort();
    result
}

/// Reports store readiness transitions and storage pressure.
async fn watch_store(app: Arc<http::App>) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous = None;
    loop {
        interval.tick().await;
        let store = app.cache.store();
        if let Some(gc) = &app.downloads_gc {
            match gc.collect(&app.cache, store::now()).await {
                Ok(report) if report["failed_objects"] == 0 => {
                    if report["removed_objects"].as_u64().unwrap_or(0) > 0 {
                        tracing::info!(
                            module = "cache",
                            scope = "io",
                            event.name = "cache.store.evicted",
                            outcome = "ok"
                        );
                    }
                }
                _ => {
                    tracing::error!(module="cache",scope="io",event.name="cache.store.degraded",available=false,error.type="store_io",outcome="error")
                }
            }
        }
        if let Some(builds) = &app.builds {
            match builds.collect(store::now()).await {
                Ok(report) if report.failed_objects == 0 => {}
                _ => {
                    tracing::error!(module = "cache", scope = "io", event.name = "cache.failed", route = builds::ROUTE, error.type = "store_io", outcome = "error")
                }
            }
        }
        let ready = store.ready().await
            && match &app.builds {
                Some(builds) => builds.ready().await,
                None => true,
            };
        if previous != Some(ready) {
            if ready {
                tracing::info!(
                    module = "cache",
                    scope = "io",
                    event.name = "cache.store.ready",
                    available = true,
                    outcome = "ready"
                );
            } else {
                tracing::error!(module = "cache", scope = "io", event.name = "cache.store.degraded", available = false, error.type = "store_unavailable", outcome = "unavailable");
            }
            previous = Some(ready);
        }
        let usage = store.usage();
        let full = usage.bytes.saturating_mul(100) >= usage.capacity.saturating_mul(95);
        let low = store
            .free_bytes()
            .is_some_and(|free| free < usage.capacity / 20);
        if full || low {
            tracing::warn!(module = "cache", scope = "io", event.name = "cache.store.pressure", available = !low, error.type = if low { "volume_free_space" } else { "store_capacity" }, outcome = "checked");
        }
    }
}
