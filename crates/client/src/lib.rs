//! Device side of the NDS network cache: binds the public download sources of one scope's tools
//! to the cache through a loopback base, and reverses that exactly.
//!
//! A scope is the home of one user, or one CI lane whose jobs run below its prefix. Binding
//! projects one binding onto its files: managed blocks in the user configuration of Cargo, npm and
//! Bun, pip and the go command, one environment file for the tools that read only variables, and
//! the project-scoped Gradle init script. User files rank below a project's own configuration, so
//! project pins, lockfiles and private registries stay in force; a user file that names a
//! default registry of its own is refused, never overwritten. Every write is atomic, and a
//! refusal or failure leaves the scope as it was; only [`Error::Uncertain`] says it may be changed.
//!
//! A bound scope fails closed: its tools reach only the cache, which reports itself not ready
//! when it does not answer, and `unbind` is the explicit way back to direct downloads. The base is
//! the cache listener on the cache host, or elsewhere the local port of the forward that the remote
//! module owns with the device's resident identity; this crate holds no credential.

pub mod binding;
pub mod view;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod adapter;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use adapter::{Client, Scope};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use nddev_device_sync_adapter_io::{CancellationToken, IoError, LocalIo, NativeIo};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[error(transparent)]
    Io(#[from] IoError),
    /// A file of the scope configures the same thing outside the managed block.
    #[error("the scope already configures this outside the managed block")]
    Conflict,
    /// A file of the scope cannot be read as its format.
    #[error("a file of the scope is invalid")]
    Invalid,
    /// Binding needs a cache that answers on its base.
    #[error("the cache does not answer on its base")]
    NotReady,
    /// Another bind or unbind of the same scope is in progress; nothing was changed.
    #[error("another bind or unbind of this scope is in progress")]
    Busy,
    /// The scope may be changed: a change failed and could not be fully undone, or the scope
    /// could not be read back after its writes; read its status back.
    #[error("the scope may be partly changed; read its status back")]
    Uncertain,
    #[error("the network cache client is not supported on this platform")]
    UnsupportedPlatform,
}

impl From<binding::BlockError> for Error {
    fn from(error: binding::BlockError) -> Self {
        match error {
            binding::BlockError::Conflict => Self::Conflict,
            binding::BlockError::Invalid => Self::Invalid,
        }
    }
}

impl Error {
    /// Stable refusal vocabulary; also the telemetry `error.type` values.
    pub fn code(&self) -> &'static str {
        match self {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Io(error) => error.code(),
            Self::Conflict => "conflict",
            Self::Invalid => "invalid",
            Self::NotReady => "not_ready",
            Self::Busy => "busy",
            Self::Uncertain => "uncertain",
            Self::UnsupportedPlatform => "unsupported_platform",
        }
    }
}

/// Application port: callers bind a scope to a base and read its state, not files or sockets.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub trait NetworkCache {
    fn status(
        &self,
        cancellation: CancellationToken,
    ) -> impl Future<Output = Result<view::Status, Error>> + Send;
    /// Writes the bindings after the cache answered on the base.
    fn bind(
        &self,
        cancellation: CancellationToken,
    ) -> impl Future<Output = Result<view::Status, Error>> + Send;
    /// Removes exactly what binding wrote.
    fn unbind(
        &self,
        cancellation: CancellationToken,
    ) -> impl Future<Output = Result<view::Status, Error>> + Send;
}

/// Canonical compiled-in runtime manifest of the client.
pub const MANIFEST: &str = include_str!("../../../contracts/module.json");
