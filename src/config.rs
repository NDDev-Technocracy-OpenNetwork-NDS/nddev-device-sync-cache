//! Process settings from the environment, each with an explicit bound.
use std::{net::SocketAddr, path::PathBuf, time::Duration};

const GIB: u64 = 1024 * 1024 * 1024;
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8740";

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration_address")]
    Address,
    #[error("configuration_root")]
    Root,
    #[error("configuration_limit")]
    Limit,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub listen: SocketAddr,
    pub root: PathBuf,
    pub capacity: u64,
    pub max_object: u64,
    pub fetches: usize,
    pub revalidate_after: Duration,
    pub fetch_wait: Duration,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub shutdown_grace: Duration,
}

fn bounded(
    get: &impl Fn(&str) -> Option<String>,
    name: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, ConfigError> {
    match get(name) {
        None => Ok(default),
        Some(value) => value
            .parse::<u64>()
            .ok()
            .filter(|value| (min..=max).contains(value))
            .ok_or(ConfigError::Limit),
    }
}

impl Settings {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::read(|name| std::env::var(name).ok())
    }

    pub fn read(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let listen = get("NDS_CACHE_LISTEN")
            .unwrap_or_else(|| DEFAULT_LISTEN.into())
            .parse()
            .map_err(|_| ConfigError::Address)?;
        let root = PathBuf::from(get("NDS_CACHE_ROOT").ok_or(ConfigError::Root)?);
        if !root.is_absolute() {
            return Err(ConfigError::Root);
        }
        let capacity = bounded(
            &get,
            "NDS_CACHE_CAPACITY_BYTES",
            200 * GIB,
            GIB,
            64 * 1024 * GIB,
        )?;
        let max_object = bounded(
            &get,
            "NDS_CACHE_MAX_OBJECT_BYTES",
            4 * GIB,
            1024 * 1024,
            capacity,
        )?;
        let fetches = bounded(&get, "NDS_CACHE_FETCHES", 16, 1, 256)? as usize;
        let revalidate_after = Duration::from_secs(bounded(
            &get,
            "NDS_CACHE_REVALIDATE_SECONDS",
            300,
            1,
            86_400,
        )?);
        Ok(Self {
            listen,
            root,
            capacity,
            max_object,
            fetches,
            revalidate_after,
            fetch_wait: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(30),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(pairs: &[(&str, &str)]) -> Result<Settings, ConfigError> {
        Settings::read(|name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn requires_an_absolute_root_and_bounded_limits() {
        assert_eq!(read(&[]).unwrap_err(), ConfigError::Root);
        assert_eq!(
            read(&[("NDS_CACHE_ROOT", "relative")]).unwrap_err(),
            ConfigError::Root
        );
        assert_eq!(
            read(&[("NDS_CACHE_ROOT", "/srv/cache"), ("NDS_CACHE_FETCHES", "0")]).unwrap_err(),
            ConfigError::Limit
        );
        assert_eq!(
            read(&[
                ("NDS_CACHE_ROOT", "/srv/cache"),
                ("NDS_CACHE_CAPACITY_BYTES", "2147483648"),
                ("NDS_CACHE_MAX_OBJECT_BYTES", "4294967296"),
            ])
            .unwrap_err(),
            ConfigError::Limit
        );
        assert_eq!(
            read(&[
                ("NDS_CACHE_ROOT", "/srv/cache"),
                ("NDS_CACHE_LISTEN", "nowhere")
            ])
            .unwrap_err(),
            ConfigError::Address
        );
        let settings = read(&[("NDS_CACHE_ROOT", "/srv/cache")]).unwrap();
        assert_eq!(settings.listen.to_string(), DEFAULT_LISTEN);
        assert_eq!(settings.fetches, 16);
    }
}
