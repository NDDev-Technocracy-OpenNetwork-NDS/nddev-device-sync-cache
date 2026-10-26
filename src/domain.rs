//! Route allowlist, request path validation and object classes. Pure rules without I/O.

/// Longest accepted request subpath, in bytes.
pub const MAX_PATH_BYTES: usize = 512;

/// Whether a stored object stays valid or must be revalidated against its upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Immutable,
    Revalidate,
}

/// One allowlisted upstream mount. `accept` classifies a validated subpath or refuses it.
#[derive(Clone, Debug)]
pub struct Route {
    pub id: &'static str,
    pub prefix: &'static str,
    pub template: &'static str,
    pub upstream: String,
    pub accept: fn(&str) -> Option<Freshness>,
}

/// A request resolved to one route and an upstream-relative subpath.
#[derive(Debug)]
pub struct Target<'a> {
    pub route: &'a Route,
    pub subpath: String,
    pub freshness: Freshness,
}

impl Target<'_> {
    pub fn upstream_url(&self) -> String {
        format!("{}{}", self.route.upstream, self.subpath)
    }

    /// Stable store identity of the object.
    pub fn key(&self) -> String {
        format!("{}{}", self.route.id, self.subpath)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    UnknownRoute,
    InvalidPath,
    NotAllowed,
}

impl Rejection {
    pub fn class(self) -> &'static str {
        match self {
            Self::UnknownRoute => "route_unknown",
            Self::InvalidPath => "path_invalid",
            Self::NotAllowed => "path_not_allowed",
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
            accept,
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

    pub fn resolve(&self, path: &str) -> Result<Target<'_>, Rejection> {
        let route = self
            .routes
            .iter()
            .find(|route| {
                path.strip_prefix(route.prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
            })
            .ok_or(Rejection::UnknownRoute)?;
        let subpath = &path[route.prefix.len()..];
        if !valid_subpath(subpath) {
            return Err(Rejection::InvalidPath);
        }
        let freshness = (route.accept)(subpath).ok_or(Rejection::NotAllowed)?;
        Ok(Target {
            route,
            subpath: subpath.to_owned(),
            freshness,
        })
    }

    pub fn redirect_allowed(&self, scheme: &str, host: &str) -> bool {
        (scheme == "https" || (self.plain_http && scheme == "http"))
            && self.redirect_hosts.iter().any(|allowed| allowed == host)
    }
}

/// Accepts `/segment/...` with unreserved characters and percent-escapes. A segment may
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
        } else if byte.is_ascii_alphanumeric() || b"._~-+".contains(&byte) {
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

/// The sparse index configuration for a client that reached the cache at `host`. Only a
/// loopback host, the forwarded listener, is accepted, so the download URL in the generated
/// configuration can never send a client elsewhere.
pub fn crates_index_config(host: &str) -> Option<String> {
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
    (matches!(name, "127.0.0.1" | "localhost" | "[::1]") && port_valid).then(|| {
        format!("{{\"dl\":\"http://{host}/crates/{{crate}}/{{crate}}-{{version}}.crate\"}}")
    })
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
    let semver = !version.is_empty()
        && version.as_bytes()[0].is_ascii_digit()
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'));
    (crate_name(name) && semver).then_some(Freshness::Immutable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(path: &str) -> Result<(&'static str, Freshness), Rejection> {
        let routes = Routes::production();
        let target = routes.resolve(path)?;
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
        assert_eq!(class("/npm/react"), Err(Rejection::UnknownRoute));
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
    fn redirects_stay_on_allowlisted_https_hosts() {
        let routes = Routes::production();
        assert!(routes.redirect_allowed("https", "release-assets.githubusercontent.com"));
        assert!(!routes.redirect_allowed("http", "release-assets.githubusercontent.com"));
        assert!(!routes.redirect_allowed("https", "example.com"));
    }
}
