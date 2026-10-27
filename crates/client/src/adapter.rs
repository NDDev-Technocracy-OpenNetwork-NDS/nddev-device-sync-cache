//! The files of a scope and the readiness probe of its base, through adapter-io's local authority.

use crate::{
    Error, IoError, LocalIo, NetworkCache,
    binding::{self, Base, Projection, Target},
    view,
};
use nddev_device_sync_adapter_io::CancellationToken;
use std::{
    ffi::OsString,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const PROBE: Duration = Duration::from_secs(3);
/// The Gradle init script of the client contract, which builds pass with `--init-script`.
const GRADLE: &str = include_str!("../../../contracts/gradle/nds-cache.init.gradle.kts");

/// The files that one user, or one CI lane whose jobs run below its root, owns for its tools.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    home: PathBuf,
    config: PathBuf,
    cargo_home: PathBuf,
    go_env: PathBuf,
    /// A Cargo configuration that the jobs read below the scope's own, which must yield to it.
    below: Option<PathBuf>,
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
        Self::new(home, config, cargo_home, go_env, None)
    }

    /// The scope of a CI lane, read from the environment its jobs run with; see [`Scope::lane_in`].
    pub fn lane(root: &Path) -> Result<Self, Error> {
        Self::lane_in(root, |name| std::env::var_os(name))
    }

    /// The scope of a CI lane whose jobs run with `HOME` at `root` and their workspaces below it,
    /// given the variables of the jobs' environment. Cargo finds `root/.cargo/config.toml` by
    /// directory discovery, so the jobs' own `CARGO_HOME`, a store that lanes may share and
    /// maintenance may evict, is never written; a configuration there ranks below the lane's and
    /// must not name a source of its own. The other tools read their user files at the defaults
    /// below `HOME`, so `XDG_CONFIG_HOME` and `GOENV` must be unset or name those defaults.
    pub fn lane_in(
        root: &Path,
        variable: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self, Error> {
        let home = directory(root.to_owned())?;
        let config = home.join(".config");
        let cargo_home = home.join(".cargo");
        let go_env = go_env(&home, &config);
        let is = |name: &str, default: &Path| variable(name).is_none_or(|value| value == default);
        if variable("HOME").is_none_or(|value| value != home)
            || !is("XDG_CONFIG_HOME", &config)
            || !is("GOENV", &go_env)
        {
            return Err(Error::Invalid);
        }
        let below = variable("CARGO_HOME")
            .map(PathBuf::from)
            .filter(|jobs| *jobs != cargo_home)
            .map(|jobs| jobs.join("config.toml"));
        Self::new(home, config, cargo_home, go_env, below)
    }

    fn new(
        home: PathBuf,
        config: PathBuf,
        cargo_home: PathBuf,
        go_env: PathBuf,
        below: Option<PathBuf>,
    ) -> Result<Self, Error> {
        if [&home, &config, &cargo_home, &go_env]
            .into_iter()
            .chain(&below)
            .all(|path| path.is_absolute())
        {
            Ok(Self {
                home,
                config,
                cargo_home,
                go_env,
                below,
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
            Target::Npm => self.config.join("nds-cache").join("npmrc"),
            Target::Pip => pip_config(&self.home, &self.config),
            Target::Go => self.go_env.clone(),
            Target::Gradle => self
                .config
                .join("nds-cache")
                .join("nds-cache.init.gradle.kts"),
        }
    }

    /// npm's user file, which ranks above the binding's global one.
    fn npm_user(&self) -> PathBuf {
        self.home.join(".npmrc")
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
                let _lock = ScopeLock::acquire(&scope.home)?;
                // Every file is checked before any is written, so a refusal changes nothing.
                if let Some(text) = scope.below.as_deref().map(read).transpose()?.flatten() {
                    binding::yields(Target::Cargo, &text)?;
                }
                if let Some(text) = read(&scope.npm_user())? {
                    binding::defers(Target::Npm, &text)?;
                }
                let npmrc = scope.path(Target::Npm);
                let projection = Projection {
                    gradle: GRADLE,
                    npmrc: npmrc.to_str().ok_or(Error::Invalid)?,
                };
                let mut changes = Vec::new();
                for target in Target::ALL {
                    let path = scope.path(target);
                    let current = read(&path)?;
                    let updated = binding::bound(target, current.as_deref(), base, &projection)?;
                    // A file the binding grows past the limit could not be read back or unbound.
                    if updated.len() as u64 > FILE_LIMIT {
                        return Err(Error::Invalid);
                    }
                    changes.push((path, current, Some(updated)));
                }
                apply(&changes)?;
                read_back(scope, base, &changes)
            })
            .await;
        report("cache.client.bound", &bound);
        bound
    }

    async fn unbind(&self, cancellation: CancellationToken) -> Result<view::Status, Error> {
        let unbound = self
            .run(cancellation, |scope, base| {
                let _lock = ScopeLock::acquire(&scope.home)?;
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
                read_back(scope, base, &changes)
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

/// Writes every change, each only while its file still holds what the preflight read, and on a
/// failure undoes the files already written. The re-read is advisory: the scope lock keeps
/// cooperating binders out, and the re-read catches an editor that wrote since the preflight, but
/// it is no atomic compare-and-swap against every other writer.
fn apply(changes: &[(PathBuf, Option<String>, Option<String>)]) -> Result<(), Error> {
    apply_with(changes, place)
}

fn apply_with(
    changes: &[(PathBuf, Option<String>, Option<String>)],
    place: impl Fn(&Path, Option<&str>) -> Result<(), Error>,
) -> Result<(), Error> {
    for (index, (path, earlier, updated)) in changes.iter().enumerate() {
        let written = match read(path) {
            Ok(current) if current == *earlier => place(path, updated.as_deref()),
            Ok(_) => Err(Error::Conflict),
            Err(error) => Err(error),
        };
        if let Err(error) = written {
            return Err(if undo(&changes[..index], &place) {
                error
            } else {
                Error::Uncertain
            });
        }
    }
    Ok(())
}

/// Restores the files already written, the last first, each only while it still holds what this
/// change wrote: a file someone edited since stays as they left it. Every file is attempted;
/// the answer is whether all were restored.
fn undo(
    written: &[(PathBuf, Option<String>, Option<String>)],
    place: &impl Fn(&Path, Option<&str>) -> Result<(), Error>,
) -> bool {
    let mut restored = true;
    for (path, earlier, updated) in written.iter().rev() {
        let ours = matches!(read(path), Ok(current) if current == *updated);
        restored &= ours && place(path, earlier.as_deref()).is_ok();
    }
    restored
}

/// How long a bind or unbind waits for another one of the same scope before it answers `Busy`.
const SCOPE_WAIT: Duration = Duration::from_secs(10);

/// An exclusive advisory lock on the scope's home directory, so that binds and unbinds of one
/// scope, by NDS and by the CLI alike, run one at a time. Locking the directory itself leaves no
/// file behind, and the lock ends when this is dropped or the process exits.
struct ScopeLock {
    _held: std::fs::File,
}

impl ScopeLock {
    fn acquire(home: &Path) -> Result<Self, Error> {
        use rustix::{
            fs::{FlockOperation, flock},
            io::Errno,
        };
        let directory = std::fs::File::open(home).map_err(|_| IoError::Unavailable)?;
        let deadline = std::time::Instant::now() + SCOPE_WAIT;
        loop {
            match flock(&directory, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(Self { _held: directory }),
                Err(Errno::WOULDBLOCK) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(Errno::WOULDBLOCK) => return Err(Error::Busy),
                Err(_) => return Err(IoError::Unavailable.into()),
            }
        }
    }
}

/// The status once the changes were applied. Written files stand, so a failure to read the scope
/// back then is `Uncertain`, never a refusal that would claim nothing changed.
fn read_back(
    scope: &Scope,
    base: &Base,
    changes: &[(PathBuf, Option<String>, Option<String>)],
) -> Result<view::Status, Error> {
    status(scope, base).map_err(|error| {
        if changes.is_empty() {
            error
        } else {
            Error::Uncertain
        }
    })
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

/// The most a scope file may hold; the configuration files the client edits stay far below it.
const FILE_LIMIT: u64 = 1 << 20;

/// The content of a regular file of at most `FILE_LIMIT` bytes, `None` when absent; a link or
/// any other kind of file is refused, also one that replaced the file after the first look, and a
/// larger file is invalid.
fn read(path: &Path) -> Result<Option<String>, Error> {
    use rustix::{
        fs::{Mode, OFlags},
        io::Errno,
    };
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(IoError::Unavailable.into()),
        Ok(metadata) if !metadata.is_file() => return Err(Error::Conflict),
        Ok(_) => {}
    }
    // The open follows no link and waits on no pipe; the checks hold for the file it opened.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(file) => std::fs::File::from(file),
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP) => return Err(Error::Conflict),
        Err(_) => return Err(IoError::Unavailable.into()),
    };
    let metadata = file.metadata().map_err(|_| IoError::Unavailable)?;
    if !metadata.is_file() {
        return Err(Error::Conflict);
    }
    if metadata.len() > FILE_LIMIT {
        return Err(Error::Invalid);
    }
    let mut content = Vec::new();
    file.take(FILE_LIMIT + 1)
        .read_to_end(&mut content)
        .map_err(|_| IoError::Unavailable)?;
    if content.len() as u64 > FILE_LIMIT {
        return Err(Error::Invalid);
    }
    String::from_utf8(content)
        .map(Some)
        .map_err(|_| Error::Invalid)
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
    // Each write gets its own temporary file, so two writers never share one.
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = path.with_extension(format!("nds-cache.{}.{serial}.tmp", std::process::id()));
    place_via(path, content, &temporary)
}

/// Writes through a temporary file that this call creates itself, private from its first byte.
/// An existing file's permissions are applied before the rename; a new file stays private. A
/// temporary name that already exists belongs to someone else and is left as it is.
fn place_via(path: &Path, content: &str, temporary: &Path) -> Result<(), Error> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temporary)
        .map_err(|_| IoError::Unavailable)?;
    let written = (|| {
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        if let Ok(metadata) = std::fs::metadata(path) {
            std::fs::set_permissions(temporary, metadata.permissions())?;
        }
        std::fs::rename(temporary, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(temporary);
        return Err(IoError::Unavailable.into());
    }
    Ok(())
}

/// Whether the cache answers `200` to `/v1/ready` on the base within `PROBE` in total.
fn ready(base: &Base) -> bool {
    answers(base.port(), PROBE)
}

/// Whether `/v1/ready` on the loopback port answers `200` within `budget` in total. The connect,
/// every write and every read get only the time left of one deadline, so a peer that sends a byte
/// at a time cannot stretch the probe.
fn answers(port: u16, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Some(Ok(mut stream)) =
        remaining(deadline).map(|left| TcpStream::connect_timeout(&address, left))
    else {
        return false;
    };
    let request =
        format!("GET /v1/ready HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    let mut head = [0u8; 12];
    within(deadline, request.len(), |done, left| {
        stream.set_write_timeout(Some(left))?;
        stream.write(&request.as_bytes()[done..])
    }) && within(deadline, head.len(), |done, left| {
        stream.set_read_timeout(Some(left))?;
        stream.read(&mut head[done..])
    }) && remaining(deadline).is_some()
        && head.starts_with(b"HTTP/1.1 200")
}

/// The time left until `deadline`, `None` once it has passed.
fn remaining(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
}

/// Moves `length` bytes through `step`, which gets the bytes moved so far and the time left, and
/// must give up by then. It ends unfinished once the deadline has passed, or when a step fails or
/// moves nothing; an interrupted step is tried again within the same deadline.
fn within(
    deadline: Instant,
    length: usize,
    mut step: impl FnMut(usize, Duration) -> std::io::Result<usize>,
) -> bool {
    let mut done = 0;
    while done < length {
        let Some(left) = remaining(deadline) else {
            return false;
        };
        match step(done, left) {
            Ok(0) => return false,
            Ok(moved) => done += moved,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_that_changed_after_the_preflight_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(&path, "edited\n").unwrap();
        let changes = [(
            path.clone(),
            Some("before\n".to_owned()),
            Some("bound\n".to_owned()),
        )];
        assert_eq!(apply(&changes), Err(Error::Conflict));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited\n");
    }

    fn change(path: &Path) -> (PathBuf, Option<String>, Option<String>) {
        (
            path.to_owned(),
            None,
            Some(format!("bound {}", path.display())),
        )
    }

    #[test]
    fn a_failed_restore_is_uncertain_and_every_later_restore_is_still_tried() {
        let root = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| root.path().join(name))
            .collect();
        let changes: Vec<_> = paths.iter().map(|path| change(path)).collect();
        // d's write fails; the undo runs c, b, a, and c's restore fails.
        let failing = |path: &Path, content: Option<&str>| {
            if path.ends_with("d") || (path.ends_with("c") && content.is_none()) {
                Err(IoError::Unavailable.into())
            } else {
                place(path, content)
            }
        };
        assert_eq!(apply_with(&changes, failing), Err(Error::Uncertain));
        assert!(paths[2].exists(), "c could not be restored");
        assert!(
            !paths[0].exists() && !paths[1].exists(),
            "a and b were restored"
        );
        // With every restore working, the write's own error stands.
        let changes: Vec<_> = paths[..2].iter().map(|path| change(path)).collect();
        let second_fails = |path: &Path, content: Option<&str>| {
            if path.ends_with("b") && content.is_some() {
                Err(IoError::Unavailable.into())
            } else {
                place(path, content)
            }
        };
        assert_eq!(
            apply_with(&changes, second_fails),
            Err(IoError::Unavailable.into())
        );
        assert!(!paths[0].exists() && !paths[1].exists());
    }

    #[test]
    fn an_edit_made_after_the_write_survives_the_undo() {
        let root = tempfile::tempdir().unwrap();
        let (first, second) = (root.path().join("a"), root.path().join("b"));
        let changes = [change(&first), change(&second)];
        // Someone edits a after it was written, then b's write fails.
        let edited = |path: &Path, content: Option<&str>| {
            if path.ends_with("b") {
                std::fs::write(root.path().join("a"), "theirs").unwrap();
                return Err(IoError::Unavailable.into());
            }
            place(path, content)
        };
        assert_eq!(apply_with(&changes, edited), Err(Error::Uncertain));
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "theirs");
    }

    #[test]
    fn a_temporary_name_someone_else_holds_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let (path, temporary) = (
            root.path().join("config.toml"),
            root.path().join("taken.tmp"),
        );
        std::fs::write(&temporary, "theirs").unwrap();
        assert_eq!(
            place_via(&path, "bound", &temporary),
            Err(IoError::Unavailable.into())
        );
        assert_eq!(std::fs::read_to_string(&temporary).unwrap(), "theirs");
        assert!(!path.exists());
    }

    #[test]
    fn a_new_file_is_private_and_an_existing_one_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let (new, existing) = (root.path().join("new"), root.path().join("existing"));
        place(&new, Some("bound")).unwrap();
        assert_eq!(
            std::fs::metadata(&new).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::write(&existing, "own").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o644)).unwrap();
        place(&existing, Some("bound")).unwrap();
        assert_eq!(
            std::fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn a_file_above_the_limit_or_not_text_is_invalid() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        let at_limit = "#".repeat(FILE_LIMIT as usize);
        std::fs::write(&path, &at_limit).unwrap();
        assert_eq!(read(&path), Ok(Some(at_limit)));
        std::fs::write(&path, "#".repeat(FILE_LIMIT as usize + 1)).unwrap();
        assert_eq!(read(&path), Err(Error::Invalid));
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert_eq!(read(&path), Err(Error::Invalid));
        assert_eq!(read(&root.path().join("absent")), Ok(None));
        assert_eq!(read(root.path()), Err(Error::Conflict));
    }

    /// A listener that answers every connection with a ready head, one byte per `gap`.
    fn trickling(gap: Duration) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = stream.set_nodelay(true);
                    let _ = stream.read(&mut [0u8; 512]);
                    for byte in b"HTTP/1.1 200 OK\r\n\r\n" {
                        std::thread::sleep(gap);
                        if stream.write_all(&[*byte]).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        port
    }

    #[test]
    fn a_peer_that_trickles_its_answer_cannot_stretch_the_probe() {
        let port = trickling(Duration::from_millis(100));
        // Every byte comes well within the budget, the twelve of the head only after 1.2 s.
        let started = Instant::now();
        assert!(!answers(port, Duration::from_millis(400)));
        assert!(started.elapsed() < Duration::from_millis(1200));
        // Given the time for the whole head, the same peer is ready.
        assert!(answers(port, Duration::from_secs(10)));
        assert!(!answers(port, Duration::ZERO));
    }

    #[test]
    fn a_transfer_ends_at_its_deadline_or_at_a_failed_step() {
        use std::io::{Error as Failure, ErrorKind};
        let later = Instant::now() + Duration::from_secs(10);
        assert!(!within(Instant::now(), 1, |_, _| unreachable!()));
        // A timeout that cannot be set fails its step.
        assert!(!within(later, 1, |_, _| Err(Failure::from(
            ErrorKind::InvalidInput
        ))));
        assert!(!within(later, 1, |_, _| Ok(0)));
        let mut steps = Vec::new();
        assert!(within(later, 3, |done, _| {
            steps.push(done);
            if steps.len() == 1 {
                Err(Failure::from(ErrorKind::Interrupted))
            } else {
                Ok(1)
            }
        }));
        assert_eq!(steps, [0, 0, 1, 2]);
    }

    #[test]
    fn a_scope_that_cannot_be_read_back_after_a_write_is_uncertain() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().to_owned();
        let config = home.join(".config");
        let scope = Scope::new(
            home.clone(),
            config.clone(),
            home.join(".cargo"),
            go_env(&home, &config),
            None,
        )
        .unwrap();
        let cargo = scope.path(Target::Cargo);
        std::fs::create_dir_all(cargo.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(home.join("elsewhere"), &cargo).unwrap();
        let base = Base::new(1).unwrap();
        assert_eq!(status(&scope, &base), Err(Error::Conflict));
        // With nothing written the refusal stands; after a write it cannot claim no change.
        assert_eq!(read_back(&scope, &base, &[]), Err(Error::Conflict));
        assert_eq!(
            read_back(&scope, &base, &[change(&cargo)]),
            Err(Error::Uncertain)
        );
    }
}
