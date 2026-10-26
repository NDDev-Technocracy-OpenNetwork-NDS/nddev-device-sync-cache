//! Thin adapter over the shared NDS telemetry SDK: envelope, filtering, bounds and output
//! belong to the SDK; this module supplies the cache's build identity.
use nddev_device_sync_telemetry::{Config, DebugWindow, Logger};

use crate::{REPOSITORY, SOURCE_COMMIT};

pub const DEFAULT_CHANNEL: &str = "alpha";
pub const DEFAULT_STANDARDS_RELEASE: &str = "unreleased";

#[derive(Debug, thiserror::Error)]
#[error("invalid_telemetry_configuration")]
pub struct TelemetryError;

/// Public build identity reported in the envelope and by `/v1/health`.
#[derive(Clone, Debug)]
pub struct Identity {
    pub version: String,
    pub channel: String,
    pub standards_release: String,
    pub environment: String,
}

impl Identity {
    pub fn read(get: &impl Fn(&str) -> Option<String>) -> Result<Self, TelemetryError> {
        let label = |name: &str, default: &str| {
            let value = get(name).unwrap_or_else(|| default.into());
            let valid = !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte));
            valid.then_some(value).ok_or(TelemetryError)
        };
        Ok(Self {
            version: label("NDS_VERSION", env!("CARGO_PKG_VERSION"))?,
            channel: label("NDS_RELEASE_CHANNEL", DEFAULT_CHANNEL)?,
            standards_release: label("NDS_STANDARDS_RELEASE", DEFAULT_STANDARDS_RELEASE)?,
            environment: label("NDS_ENVIRONMENT", "self-hosted")?,
        })
    }
}

/// The SDK configuration of this service; tests install it with their own writer.
pub fn config(identity: &Identity, debug: Option<DebugWindow>) -> Config {
    Config {
        service: "nddev-device-sync-cache".into(),
        version: identity.version.clone(),
        repository: REPOSITORY.into(),
        commit: SOURCE_COMMIT.into(),
        channel: identity.channel.clone(),
        standards: identity.standards_release.clone(),
        environment: identity.environment.clone(),
        target_prefixes: vec!["nddev_device_sync_cache".into()],
        default_module: "cache".into(),
        debug,
    }
}

/// Installs the process subscriber. Events reach the pipeline through the container log
/// collector; the returned logger emits the final stop event when dropped.
pub fn install(
    identity: &Identity,
    get: &impl Fn(&str) -> Option<String>,
) -> Result<Logger, TelemetryError> {
    let debug = DebugWindow::from_settings(get).map_err(|_| TelemetryError)?;
    nddev_device_sync_telemetry::install(config(identity, debug)).map_err(|_| TelemetryError)
}
