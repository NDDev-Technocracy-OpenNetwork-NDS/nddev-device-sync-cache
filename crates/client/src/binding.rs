//! Pure binding rules: the projection of one binding onto the files of a scope.
//!
//! Tools that read a user configuration file get a managed block in it, because a user file has
//! lower precedence than a project's own configuration, so a project that names a private
//! registry keeps it. Tools that only read variables get them from one environment file. A user
//! file that already names a default registry of its own is refused, never overwritten.

use std::fmt::Write as _;

/// The first and last line of every block this module manages in a file it shares with others.
pub const BEGIN: &str =
    "# >>> nds-cache: managed by nddev-device-sync-cache-client; edit outside this block";
pub const END: &str = "# <<< nds-cache";
/// Inside a block: the file did not exist before binding, so unbinding removes it when nothing
/// else remains.
const CREATED: &str = "# nds-cache created this file";

/// A loopback base address of the cache, `http://127.0.0.1:PORT`. The cache host itself uses its
/// listener port; a device uses the local port of its RDS forward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Base {
    port: u16,
}

impl Base {
    pub fn new(port: u16) -> Option<Self> {
        (port > 0).then_some(Self { port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// The variables of tools that read no configuration file, as `contracts/clients.json` lists
/// them; `NDS_CACHE_BASE` feeds the project-scoped Gradle init script.
pub const ENVIRONMENT: [(&str, &str); 7] = [
    ("RUSTUP_DIST_SERVER", "/rustup"),
    ("RUSTUP_UPDATE_ROOT", "/rustup/rustup"),
    ("FLUTTER_STORAGE_BASE_URL", "/flutter"),
    ("UV_PYTHON_INSTALL_MIRROR", "/python-build-standalone"),
    ("PLAYWRIGHT_DOWNLOAD_HOST", "/playwright"),
    ("PUB_HOSTED_URL", "/pub"),
    ("NDS_CACHE_BASE", ""),
];

/// One file of the projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// A POSIX shell file of `export` lines that the development profile sources.
    Environment,
    /// Cargo's `config.toml`: the crates.io source replacement.
    Cargo,
    /// `.npmrc`, which npm and Bun both read: the default registry.
    Npm,
    /// `pip.conf`: the global index.
    Pip,
    /// `uv.toml`: the default index.
    Uv,
    /// The go command's environment file: `GOPROXY`, without a direct fallback.
    Go,
    /// The Gradle init script that builds pass with `--init-script`.
    Gradle,
}

impl Target {
    pub const ALL: [Self; 7] = [
        Self::Environment,
        Self::Cargo,
        Self::Npm,
        Self::Pip,
        Self::Uv,
        Self::Go,
        Self::Gradle,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Cargo => "cargo",
            Self::Npm => "npm",
            Self::Pip => "pip",
            Self::Uv => "uv",
            Self::Go => "go",
            Self::Gradle => "gradle",
        }
    }

    /// Whether the module owns the whole file rather than one block in it.
    fn owned(self) -> bool {
        matches!(self, Self::Environment | Self::Gradle)
    }

    /// The managed content for the base, without the block markers.
    fn content(self, base: &Base, gradle: &str) -> String {
        let url = base.url();
        match self {
            Self::Environment => {
                ENVIRONMENT
                    .iter()
                    .fold(String::new(), |mut text, (name, path)| {
                        let _ = writeln!(text, "export {name}='{url}{path}'");
                        text
                    })
            }
            Self::Cargo => format!(
                "[source.crates-io]\nreplace-with = \"nds-cache\"\n\n[source.nds-cache]\nregistry = \"sparse+{url}/crates-index/\"\n"
            ),
            Self::Npm => format!("registry={url}/npm/\n"),
            Self::Pip => format!("[global]\nindex-url = {url}/pypi/simple/\n"),
            Self::Uv => format!("[[index]]\nurl = \"{url}/pypi/simple/\"\ndefault = true\n"),
            Self::Go => format!("GOPROXY={url}/goproxy\n"),
            Self::Gradle => gradle.to_owned(),
        }
    }

    /// Refuses a file whose own content already sets what the block would.
    fn check(self, rest: &str) -> Result<(), BlockError> {
        let keys = || {
            rest.lines().filter_map(|line| {
                let line = line.trim();
                (!line.starts_with(['#', ';']))
                    .then(|| line.split_once('=').map(|(key, _)| key.trim()))?
            })
        };
        match self {
            Self::Environment | Self::Gradle => Ok(()),
            Self::Cargo => {
                let table = toml_table(rest)?;
                let sources = table.get("source").and_then(toml::Value::as_table);
                refuse(sources.is_some_and(|sources| {
                    sources.contains_key("crates-io") || sources.contains_key("nds-cache")
                }))
            }
            Self::Npm => refuse(keys().any(|key| key.eq_ignore_ascii_case("registry"))),
            // A second `[global]` section is invalid for pip; its other sections stay its own.
            Self::Pip => refuse(
                rest.lines()
                    .any(|line| line.trim().eq_ignore_ascii_case("[global]")),
            ),
            Self::Uv => {
                let table = toml_table(rest)?;
                let default_index = table
                    .get("index")
                    .and_then(toml::Value::as_array)
                    .is_some_and(|indexes| {
                        indexes.iter().any(|index| {
                            index.get("default").and_then(toml::Value::as_bool) == Some(true)
                        })
                    });
                let pip_index = table
                    .get("pip")
                    .and_then(toml::Value::as_table)
                    .is_some_and(|pip| pip.contains_key("index-url"));
                refuse(table.contains_key("index-url") || default_index || pip_index)
            }
            Self::Go => refuse(keys().any(|key| key == "GOPROXY")),
        }
    }

    /// Whether the format of the file must still parse after the change.
    fn toml(self) -> bool {
        matches!(self, Self::Cargo | Self::Uv)
    }
}

fn refuse(conflict: bool) -> Result<(), BlockError> {
    if conflict {
        Err(BlockError::Conflict)
    } else {
        Ok(())
    }
}

fn toml_table(text: &str) -> Result<toml::Table, BlockError> {
    text.parse().map_err(|_| BlockError::Invalid)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BlockError {
    /// The file configures the same thing itself, or is not one this module wrote.
    #[error("the file already configures this outside the managed block")]
    Conflict,
    /// The file is not valid in its format, or holds an unterminated managed block.
    #[error("the file cannot be read as its format")]
    Invalid,
}

/// The file without the managed block, and whether the block marked the file as created by it.
fn split(text: &str) -> Result<(String, Option<bool>), BlockError> {
    let Some(start) = text.find(BEGIN) else {
        return Ok((text.to_owned(), None));
    };
    let end = text[start..].find(END).ok_or(BlockError::Invalid)? + start + END.len();
    let created = text[start..end].contains(CREATED);
    let end = text[end..].strip_prefix('\n').map_or(end, |_| end + 1);
    Ok((format!("{}{}", &text[..start], &text[end..]), Some(created)))
}

/// The new content of a file of the projection, given its current content or `None` when absent.
pub fn bound(
    target: Target,
    current: Option<&str>,
    base: &Base,
    gradle: &str,
) -> Result<String, BlockError> {
    let content = target.content(base, gradle);
    if target.owned() {
        return match current {
            None => Ok(owned_content(target, content)),
            Some(text) if is_owned(target, text, gradle) => Ok(owned_content(target, content)),
            Some(_) => Err(BlockError::Conflict),
        };
    }
    let (rest, created) = match current {
        None => (String::new(), Some(true)),
        Some(text) => split(text).map(|(rest, created)| (rest, created.or(Some(false))))?,
    };
    target.check(&rest)?;
    let mut block = format!("{BEGIN}\n");
    if created == Some(true) {
        block.push_str(CREATED);
        block.push('\n');
    }
    block.push_str(&content);
    block.push_str(END);
    block.push('\n');
    let separator = if rest.is_empty() || rest.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let updated = format!("{rest}{separator}{block}");
    if target.toml() {
        toml_table(&updated)?;
    }
    Ok(updated)
}

/// The content after unbinding: `None` removes the file, which the binding created or owns.
pub fn unbound(target: Target, current: &str, gradle: &str) -> Result<Option<String>, BlockError> {
    if target.owned() {
        return if is_owned(target, current, gradle) {
            Ok(None)
        } else {
            Err(BlockError::Conflict)
        };
    }
    match split(current)? {
        (rest, Some(true)) if rest.trim().is_empty() => Ok(None),
        (rest, _) => Ok(Some(rest)),
    }
}

/// Whether the file holds the binding.
pub fn is_bound(target: Target, current: &str, gradle: &str) -> bool {
    if target.owned() {
        is_owned(target, current, gradle)
    } else {
        current.contains(BEGIN)
    }
}

fn owned_content(target: Target, content: String) -> String {
    match target {
        // The script must stay byte-identical to the published contract.
        Target::Gradle => content,
        _ => format!("{BEGIN}\n{content}{END}\n"),
    }
}

fn is_owned(target: Target, text: &str, gradle: &str) -> bool {
    match target {
        Target::Gradle => text == gradle,
        _ => text.starts_with(BEGIN),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRADLE: &str = "// gradle init\n";

    fn base() -> Base {
        Base::new(18740).unwrap()
    }

    fn bind(target: Target, current: Option<&str>) -> Result<String, BlockError> {
        bound(target, current, &base(), GRADLE)
    }

    #[test]
    fn the_environment_names_only_tools_without_a_configuration_file() {
        let text = bind(Target::Environment, None).unwrap();
        assert!(text.contains("export PUB_HOSTED_URL='http://127.0.0.1:18740/pub'\n"));
        assert!(text.contains("export NDS_CACHE_BASE='http://127.0.0.1:18740'\n"));
        assert_eq!(text.matches("export ").count(), ENVIRONMENT.len());
        assert!(!text.contains("npm") && !text.contains("GOPROXY") && !text.contains("simple"));
        assert!(Base::new(0).is_none());
    }

    #[test]
    fn blocks_replace_themselves_and_unbinding_restores_the_file() {
        let own = "[build]\njobs = 4\n";
        let once = bind(Target::Cargo, Some(own)).unwrap();
        assert!(once.starts_with(own) && !once.contains(CREATED));
        let moved = bound(
            Target::Cargo,
            Some(&once),
            &Base::new(18741).unwrap(),
            GRADLE,
        )
        .unwrap();
        assert_eq!(moved.matches(BEGIN).count(), 1);
        assert!(moved.contains("127.0.0.1:18741") && !moved.contains("127.0.0.1:18740"));
        assert_eq!(
            unbound(Target::Cargo, &moved, GRADLE).unwrap().as_deref(),
            Some(own)
        );
        // A file the binding created is removed again; one it found empty stays.
        let created = bind(Target::Npm, None).unwrap();
        assert!(created.contains(CREATED));
        assert_eq!(unbound(Target::Npm, &created, GRADLE).unwrap(), None);
        let found_empty = bind(Target::Npm, Some("")).unwrap();
        assert_eq!(
            unbound(Target::Npm, &found_empty, GRADLE)
                .unwrap()
                .as_deref(),
            Some("")
        );
        let unterminated = format!("{own}{BEGIN}\nregistry = 1\n");
        assert_eq!(
            unbound(Target::Cargo, &unterminated, GRADLE),
            Err(BlockError::Invalid)
        );
    }

    #[test]
    fn a_default_registry_of_the_user_is_refused_not_overwritten() {
        for (target, foreign) in [
            (
                Target::Cargo,
                "[source.crates-io]\nreplace-with = \"mirror\"\n",
            ),
            (
                Target::Cargo,
                "source.crates-io.replace-with = \"m\"\n[source.m]\nregistry = \"sparse+https://m/\"\n",
            ),
            (Target::Npm, "registry = https://npm.example.internal/\n"),
            (Target::Npm, "Registry=https://npm.example.internal/\n"),
            (Target::Pip, "[global]\ntimeout = 60\n"),
            (
                Target::Uv,
                "index-url = \"https://pypi.example.internal/simple\"\n",
            ),
            (
                Target::Uv,
                "[[index]]\nname = \"corp\"\nurl = \"https://pypi.example.internal/simple\"\ndefault = true\n",
            ),
            (
                Target::Uv,
                "[pip]\nindex-url = \"https://pypi.example.internal/simple\"\n",
            ),
            (Target::Go, "GOPROXY=https://goproxy.example.internal\n"),
        ] {
            assert_eq!(
                bind(target, Some(foreign)),
                Err(BlockError::Conflict),
                "{foreign}"
            );
        }
        // Scoped registries, other pip sections and other indexes stay the user's own.
        for (target, own) in [
            (
                Target::Npm,
                "@corp:registry=https://npm.example.internal/\n# registry=old\n",
            ),
            (
                Target::Pip,
                "[install]\nindex-url = https://pypi.example.internal/simple\n",
            ),
            (
                Target::Uv,
                "[[index]]\nname = \"corp\"\nurl = \"https://pypi.example.internal/simple\"\n",
            ),
            (Target::Go, "GOPRIVATE=example.internal\n"),
        ] {
            let updated = bind(target, Some(own)).unwrap();
            assert!(updated.starts_with(own), "{own}");
        }
        assert_eq!(
            bind(Target::Cargo, Some("[build\n")),
            Err(BlockError::Invalid)
        );
    }

    #[test]
    fn owned_files_are_refused_when_someone_else_wrote_them() {
        assert_eq!(bind(Target::Gradle, None).unwrap(), GRADLE);
        assert_eq!(
            bind(Target::Gradle, Some("// edited\n")),
            Err(BlockError::Conflict)
        );
        assert_eq!(
            bind(Target::Environment, Some("export A=1\n")),
            Err(BlockError::Conflict)
        );
        assert_eq!(unbound(Target::Gradle, GRADLE, GRADLE), Ok(None));
        assert_eq!(
            unbound(Target::Gradle, "// edited\n", GRADLE),
            Err(BlockError::Conflict)
        );
    }

    #[test]
    fn the_go_proxy_has_no_direct_fallback() {
        let go = bind(Target::Go, None).unwrap();
        assert!(go.contains("GOPROXY=http://127.0.0.1:18740/goproxy\n"));
        assert!(!go.contains("direct") && !go.contains(','));
    }

    #[test]
    fn every_setting_matches_the_client_contract() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../../contracts/clients.json")).unwrap();
        let listed = |name: &str, expected: &str| {
            contract["routes"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|route| route["clients"].as_array().unwrap().iter())
                .any(|client| client["setting"][name].as_str() == Some(expected))
        };
        for (name, path) in ENVIRONMENT
            .iter()
            .filter(|(name, _)| *name != "NDS_CACHE_BASE")
        {
            assert!(listed(name, &format!("{{base}}{path}")), "{name}");
        }
        // The configuration files carry the values the contract lists for the same tools.
        for (name, value) in [
            ("source.crates-io.replace-with", "nds-cache"),
            ("source.nds-cache.registry", "sparse+{base}/crates-index/"),
            ("npm_config_registry", "{base}/npm/"),
            ("PIP_INDEX_URL", "{base}/pypi/simple/"),
            ("UV_DEFAULT_INDEX", "{base}/pypi/simple/"),
            ("GOPROXY", "{base}/goproxy"),
        ] {
            assert!(listed(name, value), "{name}");
            let written = value.replace("{base}", &base().url());
            assert!(
                Target::ALL
                    .iter()
                    .any(|target| target.content(&base(), GRADLE).contains(&written)),
                "{name}"
            );
        }
    }
}
