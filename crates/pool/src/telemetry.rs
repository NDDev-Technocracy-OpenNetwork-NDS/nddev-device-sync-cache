//! Local canonical events; no exporter, address, probe contents or client identity.
use nddev_device_sync_telemetry::Config;

pub fn config() -> Config {
    Config {
        service: "nds-forward-pool".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        repository: "NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync-cache".into(),
        commit: env!("NDS_BUILD_COMMIT").into(),
        channel: "alpha".into(),
        standards: "unreleased".into(),
        environment: "self-hosted".into(),
        target_prefixes: vec![
            "nddev_device_sync_cache_pool".into(),
            "nds_forward_pool".into(),
        ],
        default_module: "pool".into(),
        debug: None,
    }
}
