//! Explicit, bounded preparation of a public pub.dev lock, using Dart's native cache format.
//! No default binding, resolver, credential transport or shared-cache replacement is introduced.
use crate::{CancellationToken, binding::Base};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

const INPUT_LIMIT: u64 = 1024 * 1024;
const ARCHIVE_LIMIT: u64 = 64 * 1024 * 1024;
const TOTAL_LIMIT: u64 = 256 * 1024 * 1024;
const PACKAGE_LIMIT: usize = 256;
const PUBLIC: &str = "https://pub.dev";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("pub inputs or dependency source are outside the qualified public lock contract")]
    Unqualified,
    #[error("project inputs changed; no automatic restoration was attempted")]
    InputsChanged,
    #[error("an incomplete existing Pub cache is preserved; select a fresh caller-owned path")]
    FreshCacheRequired,
    #[error("Pub cache must be an explicit caller-owned directory below a protected parent")]
    CachePath,
    #[error("selected central download failed; no origin fallback is permitted")]
    Download,
    #[error("archive does not match the original lock SHA-256")]
    Digest,
    #[error("bounded package preparation limit exceeded")]
    Limit,
    #[error("native Dart operation failed")]
    Native,
    #[error("package preparation was cancelled or exceeded its native deadline")]
    Cancelled,
    #[error("package preparation I/O failed")]
    Io,
}
impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unqualified => "pub_unqualified",
            Self::InputsChanged => "pub_inputs_changed",
            Self::FreshCacheRequired => "pub_fresh_cache_required",
            Self::CachePath => "pub_cache_path",
            Self::Download => "pub_download",
            Self::Digest => "pub_digest",
            Self::Limit => "pub_limit",
            Self::Native => "pub_native",
            Self::Cancelled => "pub_cancelled",
            Self::Io => "pub_io",
        }
    }
}

pub struct Options {
    pub base: Base,
    pub project: PathBuf,
    pub cache: PathBuf,
    pub dart: PathBuf,
}
#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub status: &'static str,
    pub archives: usize,
    pub downloaded_bytes: u64,
    pub cache_created: bool,
    pub inputs_unchanged: bool,
}
#[derive(Debug, PartialEq, Eq)]
struct Archive {
    name: String,
    version: String,
    sha256: String,
}
struct Inputs {
    project: PathBuf,
    spec: Vec<u8>,
    lock: Vec<u8>,
}

fn read_regular(path: &Path) -> Result<Vec<u8>, Error> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
        .map_err(|_| Error::Unqualified)?;
    let info = file.metadata().map_err(|_| Error::Io)?;
    if !info.is_file() || info.len() > INPUT_LIMIT {
        return Err(Error::Unqualified);
    }
    let mut result = Vec::new();
    file.take(INPUT_LIMIT + 1)
        .read_to_end(&mut result)
        .map_err(|_| Error::Io)?;
    if result.len() as u64 > INPUT_LIMIT {
        return Err(Error::Unqualified);
    }
    Ok(result)
}
impl Inputs {
    fn read(project: &Path) -> Result<Self, Error> {
        let project = fs::canonicalize(project).map_err(|_| Error::Unqualified)?;
        match project.join("pubspec_overrides.yaml").symlink_metadata() {
            Ok(_) => return Err(Error::Unqualified),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Error::Unqualified),
        }
        Ok(Self {
            spec: read_regular(&project.join("pubspec.yaml"))?,
            lock: read_regular(&project.join("pubspec.lock"))?,
            project,
        })
    }
    fn check(&self) -> Result<(), Error> {
        let current = Self::read(&self.project).map_err(|_| Error::InputsChanged)?;
        if current.spec != self.spec || current.lock != self.lock {
            return Err(Error::InputsChanged);
        }
        Ok(())
    }
}

fn yaml(bytes: &[u8]) -> Result<Value, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Unqualified)?;
    serde_saphyr::from_str_with_options(
        text,
        serde_saphyr::options! {
            duplicate_keys: serde_saphyr::DuplicateKeyPolicy::Error,
            merge_keys: serde_saphyr::MergeKeyPolicy::Error,
            reject_unsupported_tags: true,
            strict_booleans: true,
            no_schema: true,
            budget: serde_saphyr::budget! {
                max_depth: 16,
                max_documents: 1,
                max_anchors: 0,
                max_aliases: 0,
                max_events: 20_000,
                max_nodes: 10_000,
                max_total_scalar_bytes: INPUT_LIMIT as usize,
            },
        },
    )
    .map_err(|_| Error::Unqualified)
}
fn object<'a>(value: &'a Value, allowed: &[&str]) -> Result<&'a Map<String, Value>, Error> {
    let map = value.as_object().ok_or(Error::Unqualified)?;
    if map.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(Error::Unqualified);
    }
    Ok(map)
}
fn text(value: Option<&Value>) -> Result<&str, Error> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(Error::Unqualified)
}
fn package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
fn sdk(value: &str) -> Result<(), Error> {
    if matches!(value, "flutter" | "dart") {
        Ok(())
    } else {
        Err(Error::Unqualified)
    }
}
/// Dependency source identity, not a version resolver. Dart validates constraints with the original lock.
fn dependency(value: &Value) -> Result<&str, Error> {
    if value.as_str().is_some_and(|s| !s.trim().is_empty()) {
        return Ok(PUBLIC);
    }
    let map = object(value, &["sdk", "hosted", "version"])?;
    if let Some(s) = map.get("sdk") {
        if map.len() != 1 {
            return Err(Error::Unqualified);
        }
        let name = text(Some(s))?;
        sdk(name)?;
        return Ok(name);
    }
    if let Some(v) = map.get("version") {
        text(Some(v))?;
    }
    let hosted = map.get("hosted").ok_or(Error::Unqualified)?;
    if hosted.as_str() != Some(PUBLIC) {
        return Err(Error::Unqualified);
    }
    Ok(PUBLIC)
}
fn qualify(spec: &[u8], lock: &[u8], hosted: Option<&str>) -> Result<Vec<Archive>, Error> {
    if hosted.is_some_and(|url| url != PUBLIC) {
        return Err(Error::Unqualified);
    }
    let spec_value = yaml(spec)?;
    let spec = object(
        &spec_value,
        &[
            "name",
            "version",
            "description",
            "publish_to",
            "homepage",
            "repository",
            "issue_tracker",
            "documentation",
            "environment",
            "dependencies",
            "dev_dependencies",
        ],
    )?;
    for key in [
        "version",
        "description",
        "publish_to",
        "homepage",
        "repository",
        "issue_tracker",
        "documentation",
    ] {
        if let Some(value) = spec.get(key) {
            text(Some(value))?;
        }
    }
    let root_name = text(spec.get("name"))?;
    if !package_name(root_name) {
        return Err(Error::Unqualified);
    }
    let environment = object(
        spec.get("environment").ok_or(Error::Unqualified)?,
        &["sdk", "flutter"],
    )?;
    text(environment.get("sdk"))?;
    for v in environment.values() {
        text(Some(v))?;
    }
    let mut direct = BTreeMap::new();
    for (field, kind) in [
        ("dependencies", "direct main"),
        ("dev_dependencies", "direct dev"),
    ] {
        if let Some(values) = spec.get(field) {
            for (name, value) in values.as_object().ok_or(Error::Unqualified)? {
                if !package_name(name)
                    || name == root_name
                    || direct
                        .insert(name.as_str(), (kind, dependency(value)?))
                        .is_some()
                {
                    return Err(Error::Unqualified);
                }
            }
        }
    }
    let lock_value = yaml(lock)?;
    let lock = object(&lock_value, &["packages", "sdks"])?;
    let sdks = object(
        lock.get("sdks").ok_or(Error::Unqualified)?,
        &["dart", "flutter"],
    )?;
    text(sdks.get("dart"))?;
    for v in sdks.values() {
        text(Some(v))?;
    }
    let packages = lock
        .get("packages")
        .and_then(Value::as_object)
        .ok_or(Error::Unqualified)?;
    if packages.len() > PACKAGE_LIMIT {
        return Err(Error::Limit);
    }
    let mut archives = Vec::new();
    for (name, entry) in packages {
        if !package_name(name) || name == root_name {
            return Err(Error::Unqualified);
        }
        let entry = object(entry, &["dependency", "description", "source", "version"])?;
        let kind = text(entry.get("dependency"))?;
        if !matches!(kind, "direct main" | "direct dev" | "transitive") {
            return Err(Error::Unqualified);
        }
        let source = text(entry.get("source"))?;
        let version = text(entry.get("version"))?;
        if semver::Version::parse(version).is_err() {
            return Err(Error::Unqualified);
        }
        let identity = match source {
            "hosted" => {
                let description = object(
                    entry.get("description").ok_or(Error::Unqualified)?,
                    &["name", "url", "sha256"],
                )?;
                let sha = text(description.get("sha256"))?;
                if text(description.get("name"))? != name
                    || text(description.get("url"))? != PUBLIC
                    || sha.len() != 64
                    || !sha
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(Error::Unqualified);
                }
                archives.push(Archive {
                    name: name.clone(),
                    version: version.to_owned(),
                    sha256: sha.to_owned(),
                });
                PUBLIC
            }
            "sdk" => {
                let s = text(entry.get("description"))?;
                sdk(s)?;
                s
            }
            _ => return Err(Error::Unqualified),
        };
        match direct.remove(name.as_str()) {
            Some((expected, expected_source))
                if expected == kind && expected_source == identity => {}
            None if kind == "transitive" => {}
            _ => return Err(Error::Unqualified),
        }
    }
    if !direct.is_empty() {
        return Err(Error::Unqualified);
    }
    Ok(archives)
}
fn owned_directory(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|m| {
        m.is_dir() && m.uid() == rustix::process::geteuid().as_raw() && m.mode() & 0o022 == 0
    })
}
fn cache_path(cache: &Path) -> Result<bool, Error> {
    if !cache.is_absolute()
        || cache.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(Error::CachePath);
    }
    let parent = cache.parent().ok_or(Error::CachePath)?;
    if fs::canonicalize(parent).ok().as_deref() != Some(parent) || !owned_directory(parent) {
        return Err(Error::CachePath);
    }
    match cache.symlink_metadata() {
        Ok(_) if owned_directory(cache) => Ok(true),
        Ok(_) => Err(Error::CachePath),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(Error::CachePath),
    }
}

struct NativeGroup {
    pid: rustix::process::Pid,
    armed: bool,
}
impl Drop for NativeGroup {
    fn drop(&mut self) {
        if self.armed {
            let _ = rustix::process::kill_process_group(self.pid, rustix::process::Signal::KILL);
        }
    }
}

async fn native(
    options: &Options,
    inputs: &Inputs,
    args: &[&std::ffi::OsStr],
    cancellation: &CancellationToken,
) -> Result<bool, Error> {
    inputs.check()?;
    let mut command = tokio::process::Command::new(&options.dart);
    command
        .args(args)
        .current_dir(&inputs.project)
        .env("PUB_CACHE", &options.cache)
        .env("DART_SUPPRESS_ANALYTICS", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let mut child = command.spawn().map_err(|_| Error::Native)?;
    let pid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
        .ok_or(Error::Native)?;
    let mut group = NativeGroup { pid, armed: true };
    let status = tokio::select! {
        result = child.wait() => result.map_err(|_| Error::Native),
        _ = cancellation.cancelled() => Err(Error::Cancelled),
        _ = tokio::time::sleep(Duration::from_secs(120)) => Err(Error::Cancelled),
    };
    if status.is_err() {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        group.armed = false;
        let _ = child.wait().await;
    }
    group.armed = false;
    inputs.check()?;
    status.map(|s| s.success())
}

/// All archives are staged and checked before the first native preload. The cache stays untouched on download refusal.
pub async fn run(options: &Options, cancellation: CancellationToken) -> Result<Report, Error> {
    let inputs = Inputs::read(&options.project)?;
    let hosted = std::env::var("PUB_HOSTED_URL")
        .map(Some)
        .or_else(|e| match e {
            std::env::VarError::NotPresent => Ok(None),
            _ => Err(Error::Unqualified),
        })?;
    let archives = qualify(&inputs.spec, &inputs.lock, hosted.as_deref())?;
    if !options.dart.is_absolute() || !options.dart.is_file() {
        return Err(Error::Unqualified);
    }
    if inputs.project.starts_with(&options.cache) {
        return Err(Error::CachePath);
    }
    let get: Vec<_> = ["pub", "get", "--offline", "--enforce-lockfile"]
        .iter()
        .map(std::ffi::OsStr::new)
        .collect();
    if cache_path(&options.cache)? {
        if native(options, &inputs, &get, &cancellation).await? {
            return Ok(Report {
                status: "warm_offline",
                archives: 0,
                downloaded_bytes: 0,
                cache_created: false,
                inputs_unchanged: true,
            });
        }
        return Err(Error::FreshCacheRequired);
    }
    if cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let stage = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .map_err(|_| Error::Io)?;
    // The workspace selects rustls-no-provider elsewhere. Configure this client explicitly
    // without changing the process-global provider. Base admits only loopback HTTP and
    // redirects are disabled, so no remote TLS roots or client credentials are selected.
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| Error::Download)?
    .with_root_certificates(rustls::RootCertStore::empty())
    .with_no_client_auth();
    let client = reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|_| Error::Download)?;
    let mut files = Vec::new();
    let mut total = 0_u64;
    for archive in &archives {
        inputs.check()?;
        let url = format!(
            "{}/pub/api/archives/{}-{}.tar.gz",
            options.base.url(),
            archive.name,
            archive.version.replace('+', "%2B")
        );
        let response = tokio::select! {
            response = client.get(url).send() => response.map_err(|_| Error::Download)?,
            _ = cancellation.cancelled() => return Err(Error::Cancelled),
        };
        if response.status() != reqwest::StatusCode::OK {
            return Err(Error::Download);
        }
        if response
            .content_length()
            .is_some_and(|n| n > ARCHIVE_LIMIT || n > TOTAL_LIMIT - total)
        {
            return Err(Error::Limit);
        }
        let mut response = response;
        let mut file = tempfile::NamedTempFile::new_in(stage.path()).map_err(|_| Error::Io)?;
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        loop {
            let next = tokio::select! {
                chunk = response.chunk() => chunk.map_err(|_| Error::Download)?,
                _ = cancellation.cancelled() => return Err(Error::Cancelled),
            };
            let Some(chunk) = next else {
                break;
            };
            size += chunk.len() as u64;
            total += chunk.len() as u64;
            if size > ARCHIVE_LIMIT || total > TOTAL_LIMIT {
                return Err(Error::Limit);
            }
            digest.update(&chunk);
            file.write_all(&chunk).map_err(|_| Error::Io)?;
        }
        let actual: String = digest
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if actual != archive.sha256 {
            return Err(Error::Digest);
        }
        file.flush().map_err(|_| Error::Io)?;
        files.push(file);
    }
    inputs.check()?;
    // Atomic create never replaces a pre-existing path, even when it appeared during downloads.
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&options.cache)
        .map_err(|_| Error::FreshCacheRequired)?;
    if !owned_directory(&options.cache) {
        return Err(Error::CachePath);
    }
    if !files.is_empty() {
        let mut args: Vec<&std::ffi::OsStr> = ["pub", "cache", "preload"]
            .iter()
            .map(std::ffi::OsStr::new)
            .collect();
        args.extend(files.iter().map(|f| f.path().as_os_str()));
        if !native(options, &inputs, &args, &cancellation).await? {
            return Err(Error::Native);
        }
    }
    if !native(options, &inputs, &get, &cancellation).await? {
        return Err(Error::Native);
    }
    Ok(Report {
        status: "prepared_offline",
        archives: archives.len(),
        downloaded_bytes: total,
        cache_created: true,
        inputs_unchanged: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> String {
        "name: fixture\nenvironment:\n  sdk: '>=3.4.0 <4.0.0'\ndependencies:\n  collection: 1.19.1\n".into()
    }
    fn lock() -> String {
        format!(
            "# Generated by pub\npackages:\n  collection:\n    dependency: 'direct main'\n    description:\n      name: collection\n      sha256: '{}'\n      url: https://pub.dev\n    source: hosted\n    version: '1.19.1'\nsdks:\n  dart: '>=3.4.0 <4.0.0'\n",
            "0".repeat(64)
        )
    }
    #[test]
    fn canonical_lock_keeps_its_native_source_identity_and_hash() {
        let plan = qualify(spec().as_bytes(), lock().as_bytes(), None).unwrap();
        assert_eq!(
            plan,
            vec![Archive {
                name: "collection".into(),
                version: "1.19.1".into(),
                sha256: "0".repeat(64)
            }]
        );
        assert!(qualify(spec().as_bytes(), lock().as_bytes(), Some(PUBLIC)).is_ok());
    }
    #[test]
    fn private_git_path_overrides_workspaces_and_unknown_fields_refuse() {
        let variants = [
            spec().replace(
                "collection: 1.19.1",
                "collection: {git: https://example.invalid/repo}",
            ),
            spec().replace("collection: 1.19.1", "collection: {path: ../elsewhere}"),
            spec().replace(
                "collection: 1.19.1",
                "collection: {hosted: https://private.invalid, version: 1.19.1}",
            ),
            format!("{}dependency_overrides: {{}}\n", spec()),
            format!("{}workspace: [member]\n", spec()),
            format!("{}resolution: workspace\n", spec()),
            format!("{}unknown_future_source: true\n", spec()),
        ];
        for variant in variants {
            assert_eq!(
                qualify(variant.as_bytes(), lock().as_bytes(), None),
                Err(Error::Unqualified)
            );
        }
        assert_eq!(
            qualify(
                spec().as_bytes(),
                lock().as_bytes(),
                Some("https://private.invalid")
            ),
            Err(Error::Unqualified)
        );
    }
    #[test]
    fn locks_must_match_direct_dependencies_and_only_public_sources() {
        for invalid in [
            lock().replace("https://pub.dev", "https://private.invalid"),
            lock().replace("source: hosted", "source: git"),
            lock().replace("direct main", "transitive"),
            lock().replace("version: '1.19.1'", "version: '../escape'"),
            lock().replace(&"0".repeat(64), &"G".repeat(64)),
            lock().replace("name: collection", "name: different"),
            lock().replace(
                "    source: hosted",
                "    unexpected: field\n    source: hosted",
            ),
        ] {
            assert_eq!(
                qualify(spec().as_bytes(), invalid.as_bytes(), None),
                Err(Error::Unqualified)
            );
        }
    }
    #[test]
    fn yaml_duplicates_aliases_tags_and_multiple_documents_do_not_hide_sources() {
        for invalid in [
            format!("{}dependencies: {{collection: 1.19.1}}\n", spec()),
            spec().replace("collection: 1.19.1", "collection: &v 1.19.1"),
            spec().replace("collection: 1.19.1", "collection: !include other.yaml"),
            format!("{}---\nname: other\n", spec()),
            spec().replace(
                "collection: 1.19.1",
                "true: {git: https://private.invalid/repo}\n  'true': 1.19.1",
            ),
        ] {
            assert!(qualify(invalid.as_bytes(), lock().as_bytes(), None).is_err());
        }
    }
    #[test]
    fn prepared_sdk_rows_are_not_converted_to_downloads() {
        let spec = "name: fixture\nenvironment:\n  sdk: '>=3.4.0 <4.0.0'\ndependencies:\n  flutter: {sdk: flutter}\n";
        let lock = "packages:\n  flutter:\n    dependency: 'direct main'\n    description: flutter\n    source: sdk\n    version: '0.0.0'\nsdks:\n  dart: '>=3.4.0 <4.0.0'\n  flutter: '>=3.0.0'\n";
        assert!(
            qualify(spec.as_bytes(), lock.as_bytes(), None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            qualify(
                spec.replace("sdk: flutter", "sdk: unknown").as_bytes(),
                lock.as_bytes(),
                None
            ),
            Err(Error::Unqualified)
        );
    }
    #[test]
    fn overrides_and_symlinked_inputs_are_not_followed_and_edits_are_not_restored() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("pubspec.yaml"), spec()).unwrap();
        fs::write(root.path().join("pubspec.lock"), lock()).unwrap();
        let inputs = Inputs::read(root.path()).unwrap();
        fs::write(root.path().join("pubspec.yaml"), "owner edit\n").unwrap();
        assert_eq!(inputs.check(), Err(Error::InputsChanged));
        assert_eq!(
            fs::read_to_string(root.path().join("pubspec.yaml")).unwrap(),
            "owner edit\n"
        );
        fs::write(root.path().join("pubspec.yaml"), spec()).unwrap();
        fs::write(root.path().join("pubspec_overrides.yaml"), "").unwrap();
        assert!(Inputs::read(root.path()).is_err());
        fs::remove_file(root.path().join("pubspec_overrides.yaml")).unwrap();
        fs::rename(
            root.path().join("pubspec.lock"),
            root.path().join("original.lock"),
        )
        .unwrap();
        symlink(
            root.path().join("original.lock"),
            root.path().join("pubspec.lock"),
        )
        .unwrap();
        assert!(Inputs::read(root.path()).is_err());
    }
}
