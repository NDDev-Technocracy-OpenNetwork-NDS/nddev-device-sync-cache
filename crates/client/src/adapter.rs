//! The files of a scope and the readiness probe of its base, through adapter-io's local authority.

use crate::{
    Error, IoError, LocalIo, NetworkCache,
    binding::{self, Base, Target},
    view,
};
use nddev_device_sync_adapter_io::CancellationToken;
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};

const PROBE: Duration = Duration::from_secs(3);
/// The Gradle init script of the client contract, which builds pass with `--init-script`.
const GRADLE: &str = include_str!("../../../contracts/gradle/nds-cache.init.gradle.kts");

/// The files that one user, or one CI job acting as its own home, owns for its tools.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    home: PathBuf,
    config: PathBuf,
    cargo_home: PathBuf,
    go_env: PathBuf,
}

impl Scope {
    /// The user's own scope, located the way the tools locate it: `HOME`, `XDG_CONFIG_HOME`,
    /// `CARGO_HOME` and `GOENV` when they are set.
    pub fn user() -> Result<Self, Error> {
        let home = directory(PathBuf::from(
            std::env::var_os("HOME").ok_or(IoError::Unavailable)?,
        ))?;
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cargo"));
        let go_env = std::env::var_os("GOENV")
            .map(PathBuf::from)
            .unwrap_or_else(|| go_env(&home, &config));
        Self::new(home, config, cargo_home, go_env)
    }

    /// A scope rooted at an existing directory that a CI job or dedicated profile uses as its
    /// home: its owner sets `HOME` to it and leaves the tool variables unset.
    pub fn rooted(root: &Path) -> Result<Self, Error> {
        let home = directory(root.to_owned())?;
        let config = home.join(".config");
        let cargo_home = home.join(".cargo");
        let go_env = go_env(&home, &config);
        Self::new(home, config, cargo_home, go_env)
    }

    fn new(
        home: PathBuf,
        config: PathBuf,
        cargo_home: PathBuf,
        go_env: PathBuf,
    ) -> Result<Self, Error> {
        if [&home, &config, &cargo_home, &go_env]
            .iter()
            .all(|path| path.is_absolute())
        {
            Ok(Self {
                home,
                config,
                cargo_home,
                go_env,
            })
        } else {
            Err(IoError::InvalidConfiguration.into())
        }
    }

    /// Whether an empty directory may go when the binding's file in it does: a directory below
    /// the configuration home, or the Cargo home.
    fn removable(&self, directory: &Path) -> bool {
        (directory.starts_with(&self.config) && directory != self.config)
            || directory == self.cargo_home
    }

    pub fn path(&self, target: Target) -> PathBuf {
        match target {
            Target::Environment => self.config.join("nds-cache").join("environment.sh"),
            Target::Cargo => self.cargo_home.join("config.toml"),
            Target::Npm => self.home.join(".npmrc"),
            Target::Pip => pip_config(&self.home, &self.config),
            Target::Uv => self.config.join("uv").join("uv.toml"),
            Target::Go => self.go_env.clone(),
            Target::Gradle => self
                .config
                .join("nds-cache")
                .join("nds-cache.init.gradle.kts"),
        }
    }
}

/// An existing real directory, not a link, that the current user owns.
fn directory(path: PathBuf) -> Result<PathBuf, Error> {
    use std::os::unix::fs::MetadataExt;
    if !path.is_absolute() {
        return Err(IoError::InvalidConfiguration.into());
    }
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| IoError::Unavailable)?;
    if !metadata.is_dir() || metadata.uid() != rustix::process::getuid().as_raw() {
        return Err(Error::Conflict);
    }
    Ok(path)
}

#[cfg(target_os = "linux")]
fn go_env(_home: &Path, config: &Path) -> PathBuf {
    config.join("go").join("env")
}

#[cfg(target_os = "macos")]
fn go_env(home: &Path, _config: &Path) -> PathBuf {
    home.join("Library")
        .join("Application Support")
        .join("go")
        .join("env")
}

#[cfg(target_os = "linux")]
fn pip_config(_home: &Path, config: &Path) -> PathBuf {
    config.join("pip").join("pip.conf")
}

/// pip on macOS reads `~/Library/Application Support/pip` when it exists, else the XDG path.
#[cfg(target_os = "macos")]
fn pip_config(home: &Path, config: &Path) -> PathBuf {
    let library = home.join("Library").join("Application Support").join("pip");
    if library.is_dir() {
        library
    } else {
        config.join("pip")
    }
    .join("pip.conf")
}

/// Binds one scope to one base.
#[derive(Clone)]
pub struct Client {
    io: LocalIo,
    scope: Scope,
    base: Base,
}

impl Client {
    pub fn new(io: LocalIo, scope: Scope, base: Base) -> Self {
        Self { io, scope, base }
    }

    async fn run<T: Send + 'static>(
        &self,
        cancellation: CancellationToken,
        operation: impl FnOnce(&Scope, &Base) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let (scope, base) = (self.scope.clone(), self.base.clone());
        self.io
            .local(cancellation, move |cancellation| {
                if cancellation.is_cancelled() {
                    return Err(IoError::Cancelled);
                }
                Ok(operation(&scope, &base))
            })
            .await?
    }
}

impl NetworkCache for Client {
    async fn status(&self, cancellation: CancellationToken) -> Result<view::Status, Error> {
        self.run(cancellation, status).await
    }

    async fn bind(&self, cancellation: CancellationToken) -> Result<view::Status, Error> {
        let bound = self
            .run(cancellation, |scope, base| {
                if !ready(base) {
                    return Err(Error::NotReady);
                }
                // Every file is checked before any is written, so a refusal changes nothing.
                let mut changes = Vec::new();
                for target in Target::ALL {
                    let path = scope.path(target);
                    let current = read(&path)?;
                    let updated = binding::bound(target, current.as_deref(), base, GRADLE)?;
                    changes.push((path, current, Some(updated)));
                }
                apply(&changes)?;
                status(scope, base)
            })
            .await;
        report("cache.client.bound", &bound);
        bound
    }

    async fn unbind(&self, cancellation: CancellationToken) -> Result<view::Status, Error> {
        let unbound = self
            .run(cancellation, |scope, base| {
                let mut changes = Vec::new();
                for target in Target::ALL {
                    let path = scope.path(target);
                    if let Some(current) = read(&path)? {
                        let updated = binding::unbound(target, &current, GRADLE)?;
                        changes.push((path, Some(current), updated));
                    }
                }
                apply(&changes)?;
                // Directories that held only a removed file go with it, never the scope's own.
                for (path, _, updated) in &changes {
                    if let Some(parent) = path
                        .parent()
                        .filter(|parent| updated.is_none() && scope.removable(parent))
                    {
                        let _ = std::fs::remove_dir(parent);
                    }
                }
                status(scope, base)
            })
            .await;
        report("cache.client.unbound", &unbound);
        unbound
    }
}

fn report(name: &'static str, result: &Result<view::Status, Error>) {
    match result {
        Ok(_) => tracing::info!(event.name = name, outcome = "ok"),
        Err(error) => {
            tracing::warn!(event.name = name, outcome = "rejected", error.type = error.code())
        }
    }
}

/// Writes every change, and on a failure restores the files already changed.
fn apply(changes: &[(PathBuf, Option<String>, Option<String>)]) -> Result<(), Error> {
    for (index, (path, _, updated)) in changes.iter().enumerate() {
        if let Err(error) = place(path, updated.as_deref()) {
            for (path, earlier, _) in &changes[..index] {
                let _ = place(path, earlier.as_deref());
            }
            return Err(error);
        }
    }
    Ok(())
}

fn status(scope: &Scope, base: &Base) -> Result<view::Status, Error> {
    let mut bindings = Vec::new();
    for target in Target::ALL {
        let current = read(&scope.path(target))?;
        bindings.push(view::Binding {
            kind: target.name(),
            bound: current.is_some_and(|text| binding::is_bound(target, &text, GRADLE)),
        });
    }
    Ok(view::Status {
        port: base.port(),
        ready: ready(base),
        bindings,
    })
}

/// The content of a regular file, `None` when absent; a link is refused.
fn read(path: &Path) -> Result<Option<String>, Error> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(IoError::Unavailable.into()),
        Ok(metadata) if !metadata.is_file() => return Err(Error::Conflict),
        Ok(_) => {}
    }
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => Err(Error::Invalid),
        Err(_) => Err(IoError::Unavailable.into()),
    }
}

/// Atomically replaces a file, keeping the permissions of an existing one, or removes it.
fn place(path: &Path, content: Option<&str>) -> Result<(), Error> {
    let Some(content) = content else {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(IoError::Unavailable.into()),
        };
    };
    let parent = path.parent().ok_or(IoError::InvalidConfiguration)?;
    std::fs::create_dir_all(parent).map_err(|_| IoError::Unavailable)?;
    if std::fs::symlink_metadata(parent).is_ok_and(|metadata| !metadata.is_dir()) {
        return Err(Error::Conflict);
    }
    let temporary = path.with_extension("nds-cache.tmp");
    let written = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        if let Ok(metadata) = std::fs::metadata(path) {
            std::fs::set_permissions(&temporary, metadata.permissions())?;
        }
        std::fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
        return Err(IoError::Unavailable.into());
    }
    Ok(())
}

/// Whether the cache answers `200` to `/v1/ready` on the base, within a short deadline.
fn ready(base: &Base) -> bool {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, base.port()));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, PROBE) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(PROBE));
    let _ = stream.set_write_timeout(Some(PROBE));
    let request = format!(
        "GET /v1/ready HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        base.port()
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 12];
    stream.read_exact(&mut head).is_ok() && head.starts_with(b"HTTP/1.1 200")
}
