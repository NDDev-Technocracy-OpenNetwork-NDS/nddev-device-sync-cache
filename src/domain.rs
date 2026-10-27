//! Route allowlist, request path validation and object classes. Pure rules without I/O.
use std::borrow::Cow;

/// Longest accepted request subpath, in bytes.
pub const MAX_PATH_BYTES: usize = 512;

/// Whether a stored object stays valid or must be revalidated against its upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Immutable,
    Revalidate,
}

/// One allowlisted upstream mount. `canonical` maps a client's form of a subpath to the upstream
/// form before validation; `accept` classifies a validated subpath or refuses it.
#[derive(Clone, Debug)]
pub struct Route {
    pub id: &'static str,
    pub prefix: &'static str,
    pub template: &'static str,
    pub upstream: String,
    pub canonical: fn(&str) -> Cow<'_, str>,
    pub accept: fn(&str) -> Option<Freshness>,
    /// Set when the route's revalidated objects are index documents.
    pub index: Option<Index>,
    /// Set for an OCI distribution route: manifests are requested in every image manifest
    /// type, and content-addressed objects are checked against the digest in their path.
    pub oci: bool,
}

/// The manifest types an OCI route requests; the registry answers with the one it holds.
pub const OCI_MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

/// Index documents that name their downloads by absolute upstream URL. The cache fetches the
/// representation the client accepts and points the download links at the client's base.
#[derive(Clone, Debug)]
pub struct Index {
    /// Representations in preference order; each one is stored as its own object.
    pub media_types: &'static [&'static str],
    /// Upstream download URL prefixes, rewritten where they open a JSON string. Empty when the
    /// clients resolve the links through their configured registry themselves.
    pub links: Vec<String>,
    /// The cache path that serves those downloads.
    pub mount: &'static str,
}

/// Leaves a subpath as the client sent it.
pub fn verbatim(path: &str) -> Cow<'_, str> {
    Cow::Borrowed(path)
}

/// A request resolved to one route and an upstream-relative subpath.
#[derive(Debug)]
pub struct Target<'a> {
    pub route: &'a Route,
    pub subpath: String,
    pub freshness: Freshness,
    /// The negotiated representation of an index document.
    pub media_type: Option<&'static str>,
}

impl Target<'_> {
    pub fn upstream_url(&self) -> String {
        format!("{}{}", self.route.upstream, self.subpath)
    }

    /// Stable store identity of the object. A representation follows `#`, which a validated
    /// subpath never contains.
    pub fn key(&self) -> String {
        match self.media_type {
            Some(media_type) => format!("{}{}#{media_type}", self.route.id, self.subpath),
            None => format!("{}{}", self.route.id, self.subpath),
        }
    }

    /// The index of an index document.
    pub fn index(&self) -> Option<&Index> {
        self.media_type.and(self.route.index.as_ref())
    }

    /// The representation to request upstream.
    pub fn upstream_accept(&self) -> Option<&'static str> {
        self.media_type
            .or(self.route.oci.then_some(OCI_MANIFEST_TYPES))
    }

    /// The SHA-256 hex digest an OCI blob or manifest-by-digest path names; the stored bytes
    /// must have it.
    pub fn digest(&self) -> Option<&str> {
        if !self.route.oci {
            return None;
        }
        let hex = self.subpath.rsplit_once("/sha256:")?.1;
        (hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        .then_some(hex)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    UnknownRoute,
    InvalidPath,
    NotAllowed,
    NotAcceptable,
    HostNotLoopback,
}

impl Rejection {
    pub fn class(self) -> &'static str {
        match self {
            Self::UnknownRoute => "route_unknown",
            Self::InvalidPath => "path_invalid",
            Self::NotAllowed => "path_not_allowed",
            Self::NotAcceptable => "not_acceptable",
            Self::HostNotLoopback => "host_not_loopback",
        }
    }

    pub fn status(self) -> u16 {
        match self {
            Self::NotAcceptable => 406,
            Self::HostNotLoopback => 400,
            _ => 404,
        }
    }
}

/// The complete route table and the hosts an upstream may redirect to.
#[derive(Clone, Debug)]
pub struct Routes {
    routes: Vec<Route>,
    redirect_hosts: Vec<String>,
    plain_http: bool,
}

impl Routes {
    /// Production upstreams. Every upstream and redirect target uses HTTPS.
    pub fn production() -> Self {
        let route = |id, prefix, template, upstream: &str, accept| Route {
            id,
            prefix,
            template,
            upstream: upstream.into(),
            canonical: verbatim,
            accept,
            index: None,
            oci: false,
        };
        let oci = |id, prefix, template, upstream: &str| Route {
            oci: true,
            ..route(id, prefix, template, upstream, oci_object)
        };
        Self {
            routes: vec![
                route(
                    "rustup",
                    "/rustup",
                    "/rustup/{path}",
                    "https://static.rust-lang.org",
                    rustup,
                ),
                route(
                    "flutter",
                    "/flutter",
                    "/flutter/{path}",
                    "https://storage.googleapis.com",
                    flutter,
                ),
                route(
                    "python-build-standalone",
                    "/python-build-standalone",
                    "/python-build-standalone/{path}",
                    "https://github.com/astral-sh/python-build-standalone/releases/download",
                    python_build_standalone,
                ),
                route(
                    "playwright",
                    "/playwright",
                    "/playwright/{path}",
                    "https://cdn.playwright.dev",
                    playwright,
                ),
                route(
                    "gradle",
                    "/gradle",
                    "/gradle/{path}",
                    "https://services.gradle.org",
                    gradle,
                ),
                route("go", "/go", "/go/{path}", "https://dl.google.com/go", go),
                route(
                    "chrome-for-testing",
                    "/chrome-for-testing",
                    "/chrome-for-testing/{path}",
                    "https://storage.googleapis.com/chrome-for-testing-public",
                    chrome_for_testing,
                ),
                route(
                    "crates-index",
                    "/crates-index",
                    "/crates-index/{path}",
                    "https://index.crates.io",
                    crates_index,
                ),
                route(
                    "crates",
                    "/crates",
                    "/crates/{path}",
                    "https://static.crates.io/crates",
                    crates,
                ),
                Route {
                    canonical: npm_path,
                    index: Some(Index {
                        media_types: &["application/vnd.npm.install-v1+json", "application/json"],
                        // The documents keep their public tarball links: npm replaces the public
                        // registry host with its configured one, so package-lock.json stays
                        // resolvable without the cache.
                        links: Vec::new(),
                        mount: "/npm/",
                    }),
                    ..route(
                        "npm",
                        "/npm",
                        "/npm/{path}",
                        "https://registry.npmjs.org",
                        npm,
                    )
                },
                Route {
                    index: Some(Index {
                        media_types: &["application/vnd.pypi.simple.v1+json"],
                        links: vec!["https://files.pythonhosted.org/".into()],
                        mount: "/pypi-files/",
                    }),
                    ..route("pypi", "/pypi", "/pypi/{path}", "https://pypi.org", pypi)
                },
                route(
                    "pypi-files",
                    "/pypi-files",
                    "/pypi-files/{path}",
                    "https://files.pythonhosted.org",
                    pypi_files,
                ),
                Route {
                    index: Some(Index {
                        media_types: &["application/vnd.pub.v2+json"],
                        links: vec!["https://pub.dev/".into()],
                        mount: "/pub/",
                    }),
                    ..route("pub", "/pub", "/pub/{path}", "https://pub.dev", pub_hosted)
                },
                route(
                    "maven-central",
                    "/maven-central",
                    "/maven-central/{path}",
                    "https://repo.maven.apache.org/maven2",
                    maven,
                ),
                route(
                    "google-maven",
                    "/google-maven",
                    "/google-maven/{path}",
                    "https://dl.google.com/dl/android/maven2",
                    maven,
                ),
                // The portal redirects plugin artifacts to their content-addressed host.
                route(
                    "gradle-plugins",
                    "/gradle-plugins",
                    "/gradle-plugins/{path}",
                    "https://plugins.gradle.org/m2",
                    maven,
                ),
                // Checksum database requests stay outside: the go command then verifies against
                // sum.golang.org directly, as with proxy.golang.org itself.
                route(
                    "goproxy",
                    "/goproxy",
                    "/goproxy/{path}",
                    "https://proxy.golang.org",
                    go_module,
                ),
                // OCI pulls name the registry after the cache's own address, for example
                // `127.0.0.1:8740/docker.io/library/alpine:3`.
                Route {
                    canonical: docker_library,
                    ..oci(
                        "docker-hub",
                        "/v2/docker.io",
                        "/v2/docker.io/{path}",
                        "https://registry-1.docker.io/v2",
                    )
                },
                oci(
                    "ghcr",
                    "/v2/ghcr.io",
                    "/v2/ghcr.io/{path}",
                    "https://ghcr.io/v2",
                ),
                oci(
                    "gcr",
                    "/v2/gcr.io",
                    "/v2/gcr.io/{path}",
                    "https://gcr.io/v2",
                ),
                oci(
                    "cgr",
                    "/v2/cgr.dev",
                    "/v2/cgr.dev/{path}",
                    "https://cgr.dev/v2",
                ),
            ],
            redirect_hosts: [
                "static.rust-lang.org",
                "storage.googleapis.com",
                "github.com",
                "release-assets.githubusercontent.com",
                "objects.githubusercontent.com",
                "cdn.playwright.dev",
                "services.gradle.org",
                "downloads.gradle.org",
                "dl.google.com",
                "repo.maven.apache.org",
                "plugins.gradle.org",
                "plugins-artifacts.gradle.org",
                // OCI token services and blob storage.
                "auth.docker.io",
                "production.cloudfront.docker.com",
                "ghcr.io",
                "pkg-containers.githubusercontent.com",
                "gcr.io",
                "cgr.dev",
                ".r2.cloudflarestorage.com",
            ]
            .map(String::from)
            .to_vec(),
            plain_http: false,
        }
    }

    /// An explicit table, for isolated upstreams. `plain_http` admits `http` redirects.
    pub fn new(routes: Vec<Route>, redirect_hosts: Vec<String>, plain_http: bool) -> Self {
        Self {
            routes,
            redirect_hosts,
            plain_http,
        }
    }

    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    /// Resolves a request path. `accept` is the client's `Accept` header, which selects the
    /// representation of an index document.
    pub fn resolve(&self, path: &str, accept: Option<&str>) -> Result<Target<'_>, Rejection> {
        let route = self
            .routes
            .iter()
            .find(|route| {
                path.strip_prefix(route.prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
            })
            .ok_or(Rejection::UnknownRoute)?;
        let subpath = (route.canonical)(&path[route.prefix.len()..]);
        // An index document may be a directory URL, as PyPI project pages are.
        let checked = match route.index {
            Some(_) => subpath.strip_suffix('/').unwrap_or(&subpath),
            None => &subpath,
        };
        if !valid_subpath(checked) {
            return Err(Rejection::InvalidPath);
        }
        let freshness = (route.accept)(&subpath).ok_or(Rejection::NotAllowed)?;
        let media_type = match (&route.index, freshness) {
            (Some(index), Freshness::Revalidate) => {
                Some(negotiate(index.media_types, accept).ok_or(Rejection::NotAcceptable)?)
            }
            _ => None,
        };
        Ok(Target {
            route,
            subpath: subpath.into_owned(),
            freshness,
            media_type,
        })
    }

    /// Whether an upstream may send the cache to `host`, by a redirect or as its token service.
    /// An entry that starts with `.` admits the subdomains of that domain.
    pub fn redirect_allowed(&self, scheme: &str, host: &str) -> bool {
        (scheme == "https" || (self.plain_http && scheme == "http"))
            && self
                .redirect_hosts
                .iter()
                .any(|allowed| match allowed.strip_prefix('.') {
                    Some(domain) => host
                        .strip_suffix(domain)
                        .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.')),
                    None => allowed == host,
                })
    }
}

/// Accepts `/segment/...` with unreserved characters, `+`, `@`, `!`, `:` and percent-escapes. A
/// segment may
/// not be empty, `.` or `..` after decoding, and an escape may not decode to a separator,
/// control character or escape introducer.
fn valid_subpath(path: &str) -> bool {
    if path.len() > MAX_PATH_BYTES || !path.starts_with('/') {
        return false;
    }
    path[1..].split('/').all(|segment| {
        let Some(decoded) = decode_segment(segment) else {
            return false;
        };
        !decoded.is_empty() && decoded != b"." && decoded != b".."
    })
}

fn decode_segment(segment: &str) -> Option<Vec<u8>> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            if value < 0x20 || value == 0x7f || matches!(value, b'/' | b'\\' | b'%') {
                return None;
            }
            decoded.push(value);
            index += 3;
        } else if byte.is_ascii_alphanumeric() || b"._~-+@!:".contains(&byte) {
            decoded.push(byte);
            index += 1;
        } else {
            return None;
        }
    }
    Some(decoded)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or_default()
}

fn rustup(path: &str) -> Option<Freshness> {
    if !(path.starts_with("/dist/") || path.starts_with("/rustup/")) {
        return None;
    }
    if file_name(path).starts_with("channel-rust-") || path == "/rustup/release-stable.toml" {
        Some(Freshness::Revalidate)
    } else {
        Some(Freshness::Immutable)
    }
}

fn flutter(path: &str) -> Option<Freshness> {
    if !(path.starts_with("/flutter_infra_release/") || path.starts_with("/download.flutter.io/")) {
        return None;
    }
    let name = file_name(path);
    if (path.starts_with("/flutter_infra_release/releases/releases_") && name.ends_with(".json"))
        || name.starts_with("maven-metadata.xml")
    {
        Some(Freshness::Revalidate)
    } else {
        Some(Freshness::Immutable)
    }
}

fn python_build_standalone(path: &str) -> Option<Freshness> {
    let mut segments = path[1..].split('/');
    let (Some(tag), Some(_file), None) = (segments.next(), segments.next(), segments.next()) else {
        return None;
    };
    (tag.len() == 8 && tag.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some(Freshness::Immutable)
}

fn playwright(path: &str) -> Option<Freshness> {
    (path.starts_with("/builds/") || path.starts_with("/dbazure/download/playwright/builds/"))
        .then_some(Freshness::Immutable)
}

fn gradle(path: &str) -> Option<Freshness> {
    let name = path.strip_prefix("/distributions/")?;
    (name.starts_with("gradle-")
        && !name.contains('/')
        && [".zip", ".zip.sha256"]
            .iter()
            .any(|suffix| name.ends_with(suffix)))
    .then_some(Freshness::Immutable)
}

fn go(path: &str) -> Option<Freshness> {
    let name = path.strip_prefix('/')?;
    (name.starts_with("go")
        && !name.contains('/')
        && [".tar.gz", ".zip", ".tar.gz.sha256", ".zip.sha256"]
            .iter()
            .any(|suffix| name.ends_with(suffix)))
    .then_some(Freshness::Immutable)
}

const CHROME_FOR_TESTING_PLATFORMS: [&str; 6] = [
    "linux64",
    "linux-arm64",
    "mac-arm64",
    "mac-x64",
    "win32",
    "win64",
];
const CHROME_FOR_TESTING_PRODUCTS: [&str; 3] = ["chrome", "chromedriver", "chrome-headless-shell"];

/// `/{major.minor.build.patch}/{platform}/{product}-{platform}.zip`, one release archive.
fn chrome_for_testing(path: &str) -> Option<Freshness> {
    let mut segments = path[1..].split('/');
    let (Some(version), Some(platform), Some(file), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return None;
    };
    let release = version.split('.').count() == 4
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    let archive = file
        .strip_suffix(".zip")
        .and_then(|stem| stem.strip_suffix(platform))
        .and_then(|product| product.strip_suffix('-'))
        .is_some_and(|product| CHROME_FOR_TESTING_PRODUCTS.contains(&product));
    (release && CHROME_FOR_TESTING_PLATFORMS.contains(&platform) && archive)
        .then_some(Freshness::Immutable)
}

/// The base address of a client that reached the cache at `host`. Only a loopback host, the
/// forwarded listener, is accepted, so a download URL the cache generates or rewrites can never
/// send a client elsewhere.
pub fn loopback_base(host: &str) -> Option<String> {
    // `port` keeps its leading colon, or is empty when the host names no port.
    let (name, port) = match host.strip_prefix("[::1]") {
        Some(rest) => ("[::1]", rest),
        None => host.split_at(host.find(':').unwrap_or(host.len())),
    };
    let port_valid = match port.strip_prefix(':') {
        None => port.is_empty(),
        Some(digits) => {
            digits.bytes().all(|byte| byte.is_ascii_digit())
                && digits.parse::<u16>().is_ok_and(|port| port > 0)
        }
    };
    (matches!(name, "127.0.0.1" | "localhost" | "[::1]") && port_valid)
        .then(|| format!("http://{host}"))
}

/// The sparse index configuration for a client that reached the cache at `host`.
pub fn crates_index_config(host: &str) -> Option<String> {
    loopback_base(host)
        .map(|base| format!("{{\"dl\":\"{base}/crates/{{crate}}/{{crate}}-{{version}}.crate\"}}"))
}

/// The representation to serve: the one the client's `Accept` gives the highest quality,
/// earlier `media_types` first on a tie. A request without the header accepts any.
pub fn negotiate(media_types: &[&'static str], accept: Option<&str>) -> Option<&'static str> {
    let Some(accept) = accept else {
        return media_types.first().copied();
    };
    let mut best: Option<(&'static str, f32)> = None;
    for &media_type in media_types {
        let quality = quality(media_type, accept);
        if quality > 0.0 && best.is_none_or(|(_, chosen)| quality > chosen) {
            best = Some((media_type, quality));
        }
    }
    best.map(|(media_type, _)| media_type)
}

/// The quality of `media_type` in an `Accept` value: the most specific matching range decides.
fn quality(media_type: &str, accept: &str) -> f32 {
    let kind = media_type
        .split_once('/')
        .map_or(media_type, |(kind, _)| kind);
    let mut matched: Option<(u8, f32)> = None;
    for range in accept.split(',') {
        let mut parts = range.split(';');
        let name = parts.next().unwrap_or_default().trim();
        let specificity = if name.eq_ignore_ascii_case(media_type) {
            3
        } else if name
            .strip_suffix("/*")
            .is_some_and(|range| range.eq_ignore_ascii_case(kind))
        {
            2
        } else if name == "*/*" {
            1
        } else {
            continue;
        };
        let quality = parts
            .find_map(|parameter| {
                let parameter = parameter.trim();
                parameter
                    .strip_prefix("q=")
                    .or_else(|| parameter.strip_prefix("Q="))
            })
            .map_or(Some(1.0), |value| value.parse::<f32>().ok())
            .unwrap_or(0.0);
        if matched.is_none_or(|(current, _)| specificity > current) {
            matched = Some((specificity, quality));
        }
    }
    matched.map_or(0.0, |(_, quality)| quality)
}

/// Streams an index document with its download links pointing at `{base}{mount}`. A link is
/// rewritten only where it opens a JSON string, so hashes and other values stay as upstream
/// sent them, and a link split across chunks is held until it completes.
pub struct Links {
    links: Vec<Vec<u8>>,
    replacement: Vec<u8>,
    base: String,
    pending: Vec<u8>,
}

impl Links {
    pub fn new(index: &Index, base: &str) -> Self {
        Self {
            links: index
                .links
                .iter()
                .map(|link| format!("\"{link}").into_bytes())
                .collect(),
            replacement: format!("\"{base}{}", index.mount).into_bytes(),
            base: base.to_owned(),
            pending: Vec::new(),
        }
    }

    /// The entity tag of the served document. It depends on the stored document and on the
    /// client base, so a client that reaches the cache at another address revalidates.
    pub fn tag(&self, digest: &str) -> String {
        let host = self.base.strip_prefix("http://").unwrap_or(&self.base);
        format!("\"sha256-{digest}@{host}\"")
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(self.pending.len());
        let mut at = 0;
        while let Some(quote) = self.pending[at..].iter().position(|&byte| byte == b'"') {
            out.extend_from_slice(&self.pending[at..at + quote]);
            at += quote;
            let rest = &self.pending[at..];
            if let Some(link) = self.links.iter().find(|link| rest.starts_with(link)) {
                out.extend_from_slice(&self.replacement);
                at += link.len();
            } else if self.links.iter().any(|link| link.starts_with(rest)) {
                // The rest may complete a link in the next chunk.
                self.pending.drain(..at);
                return out;
            } else {
                out.push(b'"');
                at += 1;
            }
        }
        out.extend_from_slice(&self.pending[at..]);
        self.pending.clear();
        out
    }

    pub fn finish(self) -> Vec<u8> {
        self.pending
    }
}

/// Crate names: ASCII letters, digits, `-` and `_`, starting with a letter.
fn crate_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// One sparse index file at its prefix directory: `1/a`, `2/ab`, `3/a/abc` or `ab/cd/abcd...`,
/// always lowercase. `config.json` is not proxied: the cache answers it itself.
fn crates_index(path: &str) -> Option<Freshness> {
    let segments: Vec<&str> = path[1..].split('/').collect();
    let name = *segments.last()?;
    if !crate_name(name) || name.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return None;
    }
    let placed = match (name.len(), segments.as_slice()) {
        (1, ["1", _]) | (2, ["2", _]) => true,
        (3, ["3", first, _]) => *first == &name[..1],
        (length, [first, second, _]) if length >= 4 => {
            *first == &name[..2] && *second == &name[2..4]
        }
        _ => false,
    };
    placed.then_some(Freshness::Revalidate)
}

/// `/{crate}/{crate}-{version}.crate`, the archive of one published version.
fn crates(path: &str) -> Option<Freshness> {
    let mut segments = path[1..].split('/');
    let (Some(name), Some(file), None) = (segments.next(), segments.next(), segments.next()) else {
        return None;
    };
    let version = file
        .strip_suffix(".crate")?
        .strip_prefix(name)?
        .strip_prefix('-')?;
    (crate_name(name) && semver(version)).then_some(Freshness::Immutable)
}

/// A semantic version: a leading digit, then letters, digits, `.`, `-` and `+`.
fn semver(version: &str) -> bool {
    !version.is_empty()
        && version.as_bytes()[0].is_ascii_digit()
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
}

/// npm clients request a scoped package document as `/@scope%2fname`; the registry and the
/// store use `/@scope/name`.
fn npm_path(path: &str) -> Cow<'_, str> {
    let scoped = path.strip_prefix("/@").and_then(|rest| {
        rest.split_once("%2f")
            .or_else(|| rest.split_once("%2F"))
            .filter(|(scope, _)| !scope.contains('/'))
    });
    match scoped {
        Some((scope, name)) => Cow::Owned(format!("/@{scope}/{name}")),
        None => Cow::Borrowed(path),
    }
}

/// npm package and scope names: URL-safe, not starting with `.` or `_`. Packages published
/// before names became lowercase keep their capitals.
fn npm_name(name: &str) -> bool {
    (1..=214).contains(&name.len())
        && !name.starts_with(['.', '_'])
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
}

/// `/{package}`, a package document, or `/{package}/-/{name}-{version}.tgz`, the tarball of one
/// published version, where a scoped package is `@{scope}/{name}`.
fn npm(path: &str) -> Option<Freshness> {
    let segments: Vec<&str> = path[1..].split('/').collect();
    let (name, rest) = match segments.as_slice() {
        [scope, name, rest @ ..] if scope.starts_with('@') => {
            (npm_name(&scope[1..]) && npm_name(name)).then_some((*name, rest))?
        }
        [name, rest @ ..] => npm_name(name).then_some((*name, rest))?,
        [] => return None,
    };
    match rest {
        [] => Some(Freshness::Revalidate),
        ["-", file] => {
            let version = file
                .strip_suffix(".tgz")?
                .strip_prefix(name)?
                .strip_prefix('-')?;
            semver(version).then_some(Freshness::Immutable)
        }
        _ => None,
    }
}

/// `/simple/{project}/`, the page of one project under its normalized name: lowercase letters,
/// digits and single `-` between them.
fn pypi(path: &str) -> Option<Freshness> {
    let name = path.strip_prefix("/simple/")?.strip_suffix('/')?;
    let normalized = !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    normalized.then_some(Freshness::Revalidate)
}

/// Docker Hub names official images under `library/`, which clients leave out.
fn docker_library(path: &str) -> Cow<'_, str> {
    let name = path
        .split_once("/manifests/")
        .or_else(|| path.split_once("/blobs/"))
        .map_or(path, |(name, _)| name);
    if name.len() > 1 && !name[1..].contains('/') {
        Cow::Owned(format!("/library{path}"))
    } else {
        Cow::Borrowed(path)
    }
}

/// One OCI repository path component: lowercase alphanumerics joined by `.`, `_`, `__` or
/// dashes.
fn oci_component(component: &str) -> bool {
    !component.is_empty()
        && component.as_bytes()[0].is_ascii_alphanumeric()
        && component.as_bytes()[component.len() - 1].is_ascii_alphanumeric()
        && component.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
}

/// OCI distribution pulls. `/{name}/manifests/{tag}` changes when the tag moves;
/// `/{name}/manifests/sha256:{hex}` and `/{name}/blobs/sha256:{hex}` are content-addressed.
fn oci_object(path: &str) -> Option<Freshness> {
    let (name, kind, reference) = match path.rsplit_once("/manifests/") {
        Some((name, reference)) => (name, "manifests", reference),
        None => {
            let (name, reference) = path.rsplit_once("/blobs/")?;
            (name, "blobs", reference)
        }
    };
    if !name.strip_prefix('/')?.split('/').all(oci_component) {
        return None;
    }
    let digest = reference.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    let tag = kind == "manifests"
        && (1..=128).contains(&reference.len())
        && (reference.as_bytes()[0].is_ascii_alphanumeric() || reference.starts_with('_'))
        && reference
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte));
    if digest {
        Some(Freshness::Immutable)
    } else if tag {
        Some(Freshness::Revalidate)
    } else {
        None
    }
}

/// A Go module path in the proxy's escaped form: lowercase elements, capitals written as `!`
/// and the lowercase letter, and a first element that names a domain.
fn go_module_path(module: &str) -> bool {
    let mut elements = module.split('/');
    elements.next().is_some_and(|domain| domain.contains('.'))
        && module.split('/').all(|element| {
            !element.is_empty()
                && !element.starts_with('.')
                && element.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._~-!".contains(&byte)
                })
        })
}

/// The Go module proxy protocol. `/{module}/@v/list` and `/{module}/@latest` change with
/// releases; `/{module}/@v/{version}.info`, `.mod` and `.zip` describe one version, whose
/// capitals are escaped like the module path's.
fn go_module(path: &str) -> Option<Freshness> {
    let (module, rest) = path[1..].split_once("/@")?;
    if !go_module_path(module) {
        return None;
    }
    if rest == "latest" || rest == "v/list" {
        return Some(Freshness::Revalidate);
    }
    let (version, kind) = rest.strip_prefix("v/")?.rsplit_once('.')?;
    let version_valid = version
        .strip_prefix('v')
        .is_some_and(|number| number.as_bytes().first().is_some_and(u8::is_ascii_digit))
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-+!".contains(&byte));
    (version_valid && matches!(kind, "info" | "mod" | "zip")).then_some(Freshness::Immutable)
}

/// Checksum files that Maven repositories publish beside every file.
const MAVEN_CHECKSUMS: [&str; 4] = ["md5", "sha1", "sha256", "sha512"];

/// The Maven repository layout. `{group path}/{artifact}/{version}/{artifact}-{version}{rest}`
/// is one file of a release, where `rest` is an extension or a classifier and extension;
/// `maven-metadata.xml` and its checksums list the versions and change with each release.
/// Snapshots are not served.
fn maven(path: &str) -> Option<Freshness> {
    let segments: Vec<&str> = path[1..].split('/').collect();
    if segments
        .iter()
        .any(|segment| segment.ends_with("-SNAPSHOT"))
    {
        return None;
    }
    let file = *segments.last()?;
    let metadata = file == "maven-metadata.xml"
        || file
            .strip_prefix("maven-metadata.xml.")
            .is_some_and(|checksum| MAVEN_CHECKSUMS.contains(&checksum));
    if metadata {
        return (segments.len() >= 2).then_some(Freshness::Revalidate);
    }
    let [_, .., artifact, version, file] = segments.as_slice() else {
        return None;
    };
    let rest = file
        .strip_prefix(*artifact)?
        .strip_prefix('-')?
        .strip_prefix(*version)?;
    (rest.len() > 1 && rest.starts_with(['.', '-'])).then_some(Freshness::Immutable)
}

/// Dart package names: lowercase letters, digits and `_`, not starting with a digit.
fn dart_package(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// `/api/packages/{package}`, the package document of the hosted repository API, and its
/// security advisories, which pub reports during resolution; or
/// `/api/archives/{package}-{version}.tar.gz`, the archive of one published version. A package
/// name has no `-`, so the first one starts the version; pub.dev escapes a build suffix's `+`.
fn pub_hosted(path: &str) -> Option<Freshness> {
    if let Some(rest) = path.strip_prefix("/api/packages/") {
        let name = rest.strip_suffix("/advisories").unwrap_or(rest);
        return dart_package(name).then_some(Freshness::Revalidate);
    }
    let (name, version) = path
        .strip_prefix("/api/archives/")?
        .strip_suffix(".tar.gz")?
        .split_once('-')?;
    let version = version.replace("%2B", "+").replace("%2b", "+");
    (dart_package(name) && semver(&version)).then_some(Freshness::Immutable)
}

/// `/packages/{aa}/{bb}/{rest}/{file}`: one release file, or its core metadata, at the path of
/// its BLAKE2b-256 digest.
fn pypi_files(path: &str) -> Option<Freshness> {
    let mut segments = path.strip_prefix("/packages/")?.split('/');
    let (Some(first), Some(second), Some(rest), Some(_file), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return None;
    };
    let hex = |segment: &str, length| {
        segment.len() == length
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    (hex(first, 2) && hex(second, 2) && hex(rest, 60)).then_some(Freshness::Immutable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(path: &str) -> Result<(&'static str, Freshness), Rejection> {
        let routes = Routes::production();
        let target = routes.resolve(path, None)?;
        Ok((target.route.id, target.freshness))
    }

    #[test]
    fn classifies_toolchain_objects() {
        use Freshness::*;
        assert_eq!(
            class("/rustup/dist/channel-rust-stable.toml"),
            Ok(("rustup", Revalidate))
        );
        assert_eq!(
            class("/rustup/rustup/release-stable.toml"),
            Ok(("rustup", Revalidate))
        );
        assert_eq!(
            class("/rustup/dist/2026-10-15/rust-std-1.99.1-x86_64-unknown-linux-gnu.tar.xz"),
            Ok(("rustup", Immutable))
        );
        assert_eq!(
            class("/flutter/flutter_infra_release/releases/releases_linux.json"),
            Ok(("flutter", Revalidate))
        );
        assert_eq!(
            class(
                "/python-build-standalone/20261021/cpython-3.14.9%2B20261021-x86_64-unknown-linux-gnu-install_only.tar.gz"
            ),
            Ok(("python-build-standalone", Immutable))
        );
        assert_eq!(
            class("/playwright/builds/cft/156.0.8078.4/linux64/chrome-linux64.zip"),
            Ok(("playwright", Immutable))
        );
        assert_eq!(
            class("/gradle/distributions/gradle-9.8.2-bin.zip"),
            Ok(("gradle", Immutable))
        );
        assert_eq!(
            class("/go/go1.27.3.linux-amd64.tar.gz"),
            Ok(("go", Immutable))
        );
        assert_eq!(
            class("/chrome-for-testing/156.0.8078.47/linux64/chrome-linux64.zip"),
            Ok(("chrome-for-testing", Immutable))
        );
        assert_eq!(
            class(
                "/chrome-for-testing/156.0.8078.47/mac-arm64/chrome-headless-shell-mac-arm64.zip"
            ),
            Ok(("chrome-for-testing", Immutable))
        );
    }

    #[test]
    fn rejects_paths_outside_the_allowlist() {
        assert_eq!(class("/maven/org/a.pom"), Err(Rejection::UnknownRoute));
        assert_eq!(class("/rustupx/dist/a"), Err(Rejection::UnknownRoute));
        assert_eq!(class("/rustup/other/file"), Err(Rejection::NotAllowed));
        assert_eq!(
            class("/gradle/distributions-snapshots/gradle-9.9-bin.zip"),
            Err(Rejection::NotAllowed)
        );
        assert_eq!(
            class("/python-build-standalone/latest/file"),
            Err(Rejection::NotAllowed)
        );
        assert_eq!(class("/go/doc/install"), Err(Rejection::NotAllowed));
        for path in [
            "/chrome-for-testing/156.0.8078/linux64/chrome-linux64.zip",
            "/chrome-for-testing/latest/linux64/chrome-linux64.zip",
            "/chrome-for-testing/156.0.8078.47/linux32/chrome-linux32.zip",
            "/chrome-for-testing/156.0.8078.47/linux64/chrome-win64.zip",
            "/chrome-for-testing/156.0.8078.47/linux64/other-linux64.zip",
            "/chrome-for-testing/156.0.8078.47/linux64/chrome-linux64.zip/extra",
        ] {
            assert_eq!(class(path), Err(Rejection::NotAllowed), "{path}");
        }
    }

    #[test]
    fn classifies_crates_io_objects() {
        use Freshness::*;
        for (path, freshness) in [
            ("/crates-index/1/a", Revalidate),
            ("/crates-index/2/ab", Revalidate),
            ("/crates-index/3/s/syn", Revalidate),
            ("/crates-index/to/ki/tokio", Revalidate),
            ("/crates-index/se/rd/serde_json", Revalidate),
            ("/crates/tokio/tokio-1.54.1.crate", Immutable),
            ("/crates/Inflector/Inflector-0.11.4.crate", Immutable),
            (
                "/crates/zstd-sys/zstd-sys-2.0.16+zstd.1.5.7.crate",
                Immutable,
            ),
        ] {
            let route = path[1..].split('/').next().unwrap();
            assert_eq!(class(path), Ok((route, freshness)), "{path}");
        }
        for path in [
            "/crates-index/config.json",
            "/crates-index/to/ki/Tokio",
            "/crates-index/to/kx/tokio",
            "/crates-index/3/x/syn",
            "/crates-index/1/ab",
            "/crates-index/to/tokio",
            "/crates/tokio/serde-1.0.0.crate",
            "/crates/tokio/tokio-.crate",
            "/crates/tokio/tokio-latest.crate",
            "/crates/tokio/1.54.1/download",
        ] {
            assert_eq!(class(path), Err(Rejection::NotAllowed), "{path}");
        }
    }

    #[test]
    fn the_index_configuration_points_downloads_at_the_loopback_base() {
        assert_eq!(
            crates_index_config("127.0.0.1:18740").as_deref(),
            Some(r#"{"dl":"http://127.0.0.1:18740/crates/{crate}/{crate}-{version}.crate"}"#)
        );
        for host in ["localhost:8740", "[::1]:8740", "127.0.0.1"] {
            assert!(crates_index_config(host).is_some(), "{host}");
        }
        for host in [
            "evil.example:8740",
            "127.0.0.1:0",
            "127.0.0.1:99999",
            "127.0.0.1:87a0",
            "127.0.0.1:",
            "127.0.0.1:8740/x",
            "[::1]x",
            "localhost.evil.example",
            "",
        ] {
            assert_eq!(crates_index_config(host), None, "{host}");
        }
    }

    #[test]
    fn rejects_traversal_and_unsafe_escapes() {
        for path in [
            "/rustup/dist/../../etc/passwd",
            "/rustup/dist/%2e%2e/secret",
            "/rustup/dist/a%2Fb",
            "/rustup/dist/a%5Cb",
            "/rustup/dist/a%00b",
            "/rustup/dist//double",
            "/rustup/dist/a b",
            "/rustup/dist/%zz",
            "/rustup/dist/a?query",
        ] {
            assert_eq!(class(path), Err(Rejection::InvalidPath), "{path}");
        }
        let long = format!("/rustup/dist/{}", "a".repeat(MAX_PATH_BYTES));
        assert_eq!(class(&long), Err(Rejection::InvalidPath));
    }

    #[test]
    fn classifies_npm_objects() {
        use Freshness::*;
        for (path, freshness) in [
            ("/npm/is-number", Revalidate),
            ("/npm/JSONStream", Revalidate),
            ("/npm/@sindresorhus/is", Revalidate),
            ("/npm/@sindresorhus%2fis", Revalidate),
            ("/npm/@types%2Fnode", Revalidate),
            ("/npm/is-number/-/is-number-7.0.0.tgz", Immutable),
            ("/npm/@sindresorhus/is/-/is-4.6.0.tgz", Immutable),
            (
                "/npm/typescript/-/typescript-6.1.0-dev.20261026.tgz",
                Immutable,
            ),
        ] {
            assert_eq!(class(path), Ok(("npm", freshness)), "{path}");
        }
        let routes = Routes::production();
        let scoped = routes.resolve("/npm/@sindresorhus%2fis", None).unwrap();
        assert_eq!(
            scoped.upstream_url(),
            "https://registry.npmjs.org/@sindresorhus/is"
        );
        for path in [
            "/npm/-/v1/search",
            "/npm/-/npm/v1/security/advisories/bulk",
            "/npm/is-number/",
            "/npm/.hidden",
            "/npm/@scope",
            "/npm/@/name",
            "/npm/is-number/-/other-7.0.0.tgz",
            "/npm/is-number/-/is-number-latest.tgz",
            "/npm/is-number/7.0.0",
            "/npm/@scope/name/-/scope-name-1.0.0.tgz",
        ] {
            assert_eq!(class(path), Err(Rejection::NotAllowed), "{path}");
        }
        for path in ["/npm/@scope/a%2fb", "/npm/a%2fb"] {
            assert_eq!(class(path), Err(Rejection::InvalidPath), "{path}");
        }
    }

    #[test]
    fn classifies_pypi_objects() {
        use Freshness::*;
        let file = "/pypi-files/packages/6b/34/415834bfdafca3c5f451532e8a8d9ba89a21c9743a0c59fbd0205c7f9426/six-1.16.0-py2.py3-none-any.whl";
        for (path, route, freshness) in [
            ("/pypi/simple/six/", "pypi", Revalidate),
            ("/pypi/simple/typing-extensions/", "pypi", Revalidate),
            ("/pypi/simple/zope-interface/", "pypi", Revalidate),
            (file, "pypi-files", Immutable),
        ] {
            assert_eq!(class(path), Ok((route, freshness)), "{path}");
        }
        assert_eq!(
            class(&format!("{file}.metadata")),
            Ok(("pypi-files", Immutable))
        );
        for path in [
            "/pypi/simple/",
            "/pypi/simple/six",
            "/pypi/simple/Six/",
            "/pypi/simple/typing_extensions/",
            "/pypi/simple/zope.interface/",
            "/pypi/simple/a--b/",
            "/pypi/simple/-six/",
            "/pypi/pypi/six/json",
            "/pypi-files/packages/source/s/six/six-1.16.0.tar.gz",
            "/pypi-files/packages/6B/34/415834bfdafca3c5f451532e8a8d9ba89a21c9743a0c59fbd0205c7f9426/six.whl",
            "/pypi-files/packages/6b/34/4158/six-1.16.0-py2.py3-none-any.whl",
        ] {
            assert_eq!(class(path), Err(Rejection::NotAllowed), "{path}");
        }
        assert_eq!(class("/pypi/simple//"), Err(Rejection::InvalidPath));
        assert_eq!(class("/rustup/dist/"), Err(Rejection::InvalidPath));
    }

    #[test]
    fn classifies_pub_objects() {
        use Freshness::*;
        for (path, freshness) in [
            ("/pub/api/packages/path", Revalidate),
            ("/pub/api/packages/flutter_lints", Revalidate),
            ("/pub/api/packages/_fe_analyzer_shared", Revalidate),
            ("/pub/api/packages/http/advisories", Revalidate),
            ("/pub/api/archives/path-1.9.1.tar.gz", Immutable),
            ("/pub/api/archives/flutter_lints-6.0.0.tar.gz", Immutable),
            ("/pub/api/archives/analyzer-8.1.0-dev.1.tar.gz", Immutable),
            ("/pub/api/archives/build-2.4.2%2B1.tar.gz", Immutable),
        ] {
            assert_eq!(class(path), Ok(("pub", freshness)), "{path}");
        }
        let routes = Routes::production();
        let document = routes
            .resolve(
                "/pub/api/packages/path",
                Some("application/vnd.pub.v2+json"),
            )
            .unwrap();
        assert_eq!(document.media_type, Some("application/vnd.pub.v2+json"));
        for path in [
            "/pub/api/packages/Path",
            "/pub/api/packages/1path",
            "/pub/api/packages/path/versions",
            "/pub/api/packages/path/advisories/x",
            "/pub/api/packages/path/",
            "/pub/api/archives/path.tar.gz",
            "/pub/api/archives/path-latest.tar.gz",
            "/pub/api/archives/path-1.9.1.zip",
            "/pub/packages/path/versions/1.9.1.tar.gz",
            "/pub/api/search",
        ] {
            assert_eq!(class(path), Err(Rejection::NotAllowed), "{path}");
        }
    }

    #[test]
    fn classifies_maven_objects() {
        use Freshness::*;
        for (path, route, freshness) in [
            (
                "/maven-central/com/google/code/gson/gson/2.11.0/gson-2.11.0.jar",
                "maven-central",
                Immutable,
            ),
            (
                "/maven-central/com/google/code/gson/gson/2.11.0/gson-2.11.0.pom.sha1",
                "maven-central",
                Immutable,
            ),
            (
                "/maven-central/com/google/code/gson/gson/2.11.0/gson-2.11.0-sources.jar",
                "maven-central",
                Immutable,
            ),
            (
                "/maven-central/com/google/code/gson/gson/maven-metadata.xml",
                "maven-central",
                Revalidate,
            ),
            (
                "/maven-central/com/google/code/gson/gson/maven-metadata.xml.sha256",
                "maven-central",
                Revalidate,
            ),
            (
                "/google-maven/androidx/core/core/1.13.1/core-1.13.1.aar",
                "google-maven",
                Immutable,
            ),
            (
                "/google-maven/androidx/core/core/1.13.1/core-1.13.1.module",
                "google-maven",
                Immutable,
            ),
            (
                "/gradle-plugins/org/jetbrains/kotlin/jvm/org.jetbrains.kotlin.jvm.gradle.plugin/2.0.0/org.jetbrains.kotlin.jvm.gradle.plugin-2.0.0.pom",
                "gradle-plugins",
                Immutable,
            ),
        ] {
            assert_eq!(class(path), Ok((route, freshness)), "{path}");
        }
        for path in [
            "/maven-central/com/example/app/1.0-SNAPSHOT/app-1.0-SNAPSHOT.jar",
            "/maven-central/com/example/app/1.0-SNAPSHOT/maven-metadata.xml",
            "/maven-central/com/google/code/gson/gson/2.11.0/other-2.11.0.jar",
            "/maven-central/com/google/code/gson/gson/2.11.0/gson-2.10.0.jar",
            "/maven-central/com/google/code/gson/gson/2.11.0/gson-2.11.0",
            "/maven-central/gson/2.11.0/gson-2.11.0.jar",
            "/maven-central/maven-metadata.xml",
            "/maven-central/com/google/code/gson/gson/maven-metadata.xml.asc",
            "/maven-central/archetype-catalog.xml",
        ] {
            assert_eq!(class(path), Err(Rejection::NotAllowed), "{path}");
        }
        let routes = Routes::production();
        assert!(routes.redirect_allowed("https", "plugins-artifacts.gradle.org"));
    }

    #[test]
    fn classifies_go_module_objects() {
        use Freshness::*;
        for (path, freshness) in [
            ("/goproxy/github.com/google/uuid/@v/list", Revalidate),
            ("/goproxy/github.com/google/uuid/@latest", Revalidate),
            ("/goproxy/github.com/google/uuid/@v/v1.6.0.info", Immutable),
            ("/goproxy/github.com/google/uuid/@v/v1.6.0.mod", Immutable),
            ("/goproxy/github.com/google/uuid/@v/v1.6.0.zip", Immutable),
            (
                "/goproxy/github.com/!azure/azure-sdk-for-go/@v/v68.0.0+incompatible.mod",
                Immutable,
            ),
            (
                "/goproxy/golang.org/x/mod/@v/v0.0.0-20240101123456-abcdef123456.zip",
                Immutable,
            ),
            (
                "/goproxy/golang.org/toolchain/@v/v0.0.1-go1.27.3.linux-amd64.zip",
                Immutable,
            ),
            ("/goproxy/go.yaml.in/yaml/v4/@v/v4.0.0-rc.3.info", Immutable),
        ] {
            assert_eq!(class(path), Ok(("goproxy", freshness)), "{path}");
        }
        for path in [
            "/goproxy/sumdb/sum.golang.org/supported",
            "/goproxy/sumdb/sum.golang.org/lookup/github.com/google/uuid@v1.6.0",
            "/goproxy/github.com/Google/uuid/@v/list",
            "/goproxy/uuid/@v/list",
            "/goproxy/github.com/google/uuid/@v/v1.6.0.tar.gz",
            "/goproxy/github.com/google/uuid/@v/latest.info",
            "/goproxy/github.com/google/uuid/@v/v1.6.0",
            "/goproxy/github.com/google/uuid/@v/",
        ] {
            assert!(class(path).is_err(), "{path}");
        }
        assert_eq!(
            class("/go/go1.27.3.linux-amd64.tar.gz"),
            Ok(("go", Immutable))
        );
    }

    #[test]
    fn classifies_oci_objects() {
        use Freshness::*;
        let digest = "sha256:28a33e0390da2b341d05e53803c6766977039baedfc8a82a4910d5acc4755b66";
        let routes = Routes::production();
        for (path, route, freshness, upstream) in [
            (
                "/v2/docker.io/library/alpine/manifests/3.22",
                "docker-hub",
                Revalidate,
                "https://registry-1.docker.io/v2/library/alpine/manifests/3.22",
            ),
            (
                "/v2/docker.io/alpine/manifests/3.22",
                "docker-hub",
                Revalidate,
                "https://registry-1.docker.io/v2/library/alpine/manifests/3.22",
            ),
            (
                "/v2/docker.io/grafana/grafana/manifests/latest",
                "docker-hub",
                Revalidate,
                "https://registry-1.docker.io/v2/grafana/grafana/manifests/latest",
            ),
            (
                "/v2/ghcr.io/linuxcontainers/alpine/manifests/latest",
                "ghcr",
                Revalidate,
                "https://ghcr.io/v2/linuxcontainers/alpine/manifests/latest",
            ),
            (
                "/v2/cgr.dev/chainguard/static/manifests/latest",
                "cgr",
                Revalidate,
                "https://cgr.dev/v2/chainguard/static/manifests/latest",
            ),
        ] {
            let target = routes.resolve(path, None).unwrap();
            assert_eq!(
                (target.route.id, target.freshness),
                (route, freshness),
                "{path}"
            );
            assert_eq!(target.upstream_url(), upstream);
            assert_eq!(target.upstream_accept(), Some(OCI_MANIFEST_TYPES));
            assert_eq!(target.digest(), None);
        }
        for path in [
            format!("/v2/gcr.io/distroless/static/manifests/{digest}"),
            format!("/v2/docker.io/library/alpine/blobs/{digest}"),
            format!("/v2/docker.io/alpine/blobs/{digest}"),
        ] {
            let target = routes.resolve(&path, None).unwrap();
            assert_eq!(target.freshness, Immutable, "{path}");
            assert_eq!(target.digest(), Some(&digest[7..]));
        }
        for path in [
            "/v2/docker.io/library/alpine/blobs/latest",
            "/v2/docker.io/Library/alpine/manifests/3",
            "/v2/docker.io/library/alpine/manifests/-bad",
            "/v2/docker.io/library/alpine/tags/list",
            "/v2/docker.io/library/alpine/blobs/uploads/",
            "/v2/docker.io/library/alpine/manifests/sha256:abc",
            "/v2/quay.io/coreos/etcd/manifests/latest",
        ] {
            assert!(routes.resolve(path, None).is_err(), "{path}");
        }
        assert!(
            routes.redirect_allowed("https", "docker-images-prod.6aa3.r2.cloudflarestorage.com")
        );
        assert!(!routes.redirect_allowed("https", "r2.cloudflarestorage.com"));
        assert!(!routes.redirect_allowed("https", "evilr2.cloudflarestorage.com"));
        assert!(routes.redirect_allowed("https", "auth.docker.io"));
        assert_eq!(
            routes.resolve("/npm/is-number", None).unwrap().digest(),
            None
        );
    }

    #[test]
    fn index_documents_are_stored_per_negotiated_representation() {
        let routes = Routes::production();
        let bun = "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*";
        let install = routes.resolve("/npm/is-number", Some(bun)).unwrap();
        assert_eq!(
            install.media_type,
            Some("application/vnd.npm.install-v1+json")
        );
        assert_eq!(
            install.key(),
            "npm/is-number#application/vnd.npm.install-v1+json"
        );
        let full = routes
            .resolve("/npm/is-number", Some("application/json"))
            .unwrap();
        assert_eq!(full.key(), "npm/is-number#application/json");
        let tarball = routes
            .resolve(
                "/npm/is-number/-/is-number-7.0.0.tgz",
                Some("application/json"),
            )
            .unwrap();
        assert_eq!(
            (tarball.media_type, tarball.key().as_str()),
            (None, "npm/is-number/-/is-number-7.0.0.tgz")
        );
        assert_eq!(
            routes
                .resolve("/pypi/simple/six/", Some("text/html"))
                .unwrap_err(),
            Rejection::NotAcceptable
        );
    }

    #[test]
    fn negotiation_follows_quality_and_specificity() {
        let types = &["application/vnd.pypi.simple.v1+json", "text/html"];
        for (accept, chosen) in [
            (None, Some(types[0])),
            (Some("*/*"), Some(types[0])),
            (Some("text/html"), Some(types[1])),
            (
                Some("text/html, application/vnd.pypi.simple.v1+json;q=0.2"),
                Some(types[1]),
            ),
            (
                Some("application/vnd.pypi.simple.v1+json, text/html;q=0.01"),
                Some(types[0]),
            ),
            (Some("application/*;q=0.5, text/*;q=0.9"), Some(types[1])),
            (
                Some("*/*, application/vnd.pypi.simple.v1+json;q=0"),
                Some(types[1]),
            ),
            (
                Some("application/vnd.pypi.simple.v1+json;Q=0.3, text/html;q=x"),
                Some(types[0]),
            ),
            (Some("image/png"), None),
            (Some(""), None),
        ] {
            assert_eq!(negotiate(types, accept), chosen, "{accept:?}");
        }
    }

    #[test]
    fn links_point_at_the_client_base_however_the_document_is_split() {
        let index = Index {
            media_types: &["application/json"],
            links: vec![
                "https://registry.npmjs.org/".into(),
                "http://registry.npmjs.org/".into(),
            ],
            mount: "/npm/",
        };
        let document = br#"{"versions":{"1.0.0":{"dist":{"tarball":"https://registry.npmjs.org/a/-/a-1.0.0.tgz","integrity":"sha512-x"}},"0.1.0":{"dist":{"tarball":"http://registry.npmjs.org/a/-/a-0.1.0.tgz"}}},"readme":"see https://registry.npmjs.org/a and \"https://registry.npmjs.or"}"#;
        let expected = br#"{"versions":{"1.0.0":{"dist":{"tarball":"http://127.0.0.1:18740/npm/a/-/a-1.0.0.tgz","integrity":"sha512-x"}},"0.1.0":{"dist":{"tarball":"http://127.0.0.1:18740/npm/a/-/a-0.1.0.tgz"}}},"readme":"see https://registry.npmjs.org/a and \"https://registry.npmjs.or"}"#;
        for size in 1..=document.len() {
            let mut links = Links::new(&index, "http://127.0.0.1:18740");
            let mut out = Vec::new();
            for chunk in document.chunks(size) {
                out.extend(links.push(chunk));
            }
            out.extend(links.finish());
            assert_eq!(
                String::from_utf8(out).unwrap(),
                String::from_utf8_lossy(expected),
                "chunk size {size}"
            );
        }
    }

    #[test]
    fn npm_documents_keep_their_public_links() {
        let routes = Routes::production();
        let npm = routes
            .routes()
            .iter()
            .find(|route| route.id == "npm")
            .unwrap();
        let index = npm.index.clone().unwrap();
        let document = br#"{"versions":{"1.0.0":{"dist":{"tarball":"https://registry.npmjs.org/a/-/a-1.0.0.tgz"}}}}"#;
        let mut links = Links::new(&index, "http://127.0.0.1:18740");
        let mut out = links.push(document);
        out.extend(links.finish());
        assert_eq!(out, document);
    }

    #[test]
    fn redirects_stay_on_allowlisted_https_hosts() {
        let routes = Routes::production();
        assert!(routes.redirect_allowed("https", "release-assets.githubusercontent.com"));
        assert!(!routes.redirect_allowed("http", "release-assets.githubusercontent.com"));
        assert!(!routes.redirect_allowed("https", "example.com"));
    }
}
