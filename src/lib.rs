//! NDS central network cache. Proxies an allowlist of upstream routes, keeps their objects
//! on the cache volume and reports through the shared NDS telemetry envelope.
#![forbid(unsafe_code)]

pub mod cache;
pub mod config;
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
    Ok(Arc::new(http::App { cache, identity }))
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
        let ready = store.ready().await;
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
