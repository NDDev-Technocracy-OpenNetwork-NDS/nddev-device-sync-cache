//! Pure binding rules: the projection of one binding onto the files of a scope.
//!
//! Tools that read a user configuration file get a managed block in it, because a user file has
//! lower precedence than a project's own configuration, so a project that names a private
//! registry keeps it. Tools that only read variables get them from one environment file. A user
//! file that already names a default registry of its own is refused, never overwritten. Tools whose
//! lockfiles record the index they resolved against, uv, pub and Bun, stay unbound, so a committed
//! lockfile keeps resolving and verifying as written. Bun reads npm's user file and variables, so
//! npm gets a global configuration file of its own, which the environment file names and Bun does
//! not read; the user's and the project's `.npmrc` keep their precedence over it.

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
pub const ENVIRONMENT: [(&str, &str); 6] = [
    ("RUSTUP_DIST_SERVER", "/rustup"),
    ("RUSTUP_UPDATE_ROOT", "/rustup/rustup"),
    ("FLUTTER_STORAGE_BASE_URL", "/flutter"),
    ("UV_PYTHON_INSTALL_MIRROR", "/python-build-standalone"),
    ("PLAYWRIGHT_DOWNLOAD_HOST", "/playwright"),
    ("NDS_CACHE_BASE", ""),
];

/// The variable that points npm at the binding's global configuration file.
pub const NPM_GLOBALCONFIG: &str = "npm_config_globalconfig";

/// What a projection needs besides the base: the contract's Gradle init script, and where the
/// scope keeps npm's global configuration file, which the environment file names.
#[derive(Clone, Copy, Debug)]
pub struct Projection<'a> {
    pub gradle: &'a str,
    pub npmrc: &'a str,
}

/// One file of the projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// A POSIX shell file of `export` lines that the development profile sources.
    Environment,
    /// Cargo's `config.toml`: the crates.io source replacement.
    Cargo,
    /// npm's global configuration file, which the environment file names: the default registry.
    Npm,
    /// `pip.conf`: the global index.
    Pip,
    /// The go command's environment file: `GOPROXY`, without a direct fallback.
    Go,
    /// The Gradle init script that builds pass with `--init-script`.
    Gradle,
}

impl Target {
    pub const ALL: [Self; 6] = [
        Self::Environment,
        Self::Cargo,
        Self::Npm,
        Self::Pip,
        Self::Go,
        Self::Gradle,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Cargo => "cargo",
            Self::Npm => "npm",
            Self::Pip => "pip",
            Self::Go => "go",
            Self::Gradle => "gradle",
        }
    }

    /// Whether the module owns the whole file rather than one block in it.
    fn owned(self) -> bool {
        matches!(self, Self::Environment | Self::Npm | Self::Gradle)
    }

    /// The managed content for the base, without the block markers.
    fn content(self, base: &Base, projection: &Projection) -> String {
        let url = base.url();
        match self {
            Self::Environment => {
                let mut text = ENVIRONMENT
                    .iter()
                    .fold(String::new(), |mut text, (name, path)| {
                        let _ = writeln!(text, "export {name}='{url}{path}'");
                        text
                    });
                let _ = writeln!(text, "export {NPM_GLOBALCONFIG}='{}'", projection.npmrc);
                text
            }
            Self::Cargo => format!(
                "[source.crates-io]\nreplace-with = \"nds-cache\"\n\n[source.nds-cache]\nregistry = \"sparse+{url}/crates-index/\"\n"
            ),
            Self::Npm => format!("registry={url}/npm/\n"),
            Self::Pip => format!("[global]\nindex-url = {url}/pypi/simple/\n"),
            Self::Go => format!("GOPROXY={url}/goproxy\n"),
            Self::Gradle => projection.gradle.to_owned(),
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
            // A file Cargo includes could name a source of its own; it is never followed.
            Self::Cargo => {
                let table = toml_table(rest)?;
                let sources = table.get("source").and_then(toml::Value::as_table);
                refuse(
                    table.contains_key("include")
                        || sources.is_some_and(|sources| {
                            sources.contains_key("crates-io") || sources.contains_key("nds-cache")
                        }),
                )
            }
            Self::Npm => refuse(keys().any(|key| key.eq_ignore_ascii_case("registry"))),
            // A second `[global]` section is invalid for pip; its other sections stay its own.
            Self::Pip => refuse(
                rest.lines()
                    .any(|line| line.trim().eq_ignore_ascii_case("[global]")),
            ),
            Self::Go => refuse(keys().any(|key| key == "GOPROXY")),
        }
    }

    /// Whether the format of the file must still parse after the change.
    fn toml(self) -> bool {
        matches!(self, Self::Cargo)
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
    projection: &Projection,
) -> Result<String, BlockError> {
    // The environment file quotes the path for the shell that sources it.
    if target == Target::Environment && projection.npmrc.contains(['\'', '\n']) {
        return Err(BlockError::Invalid);
    }
    let content = target.content(base, projection);
    if target.owned() {
        return match current {
            None => Ok(owned_content(target, content)),
            Some(text) if is_owned(target, text, projection.gradle) => {
                Ok(owned_content(target, content))
            }
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

/// Refuses a file of the same tool that ranks below the binding's own and names a default source
/// there: the binding would shadow it rather than leave it in force.
pub fn yields(target: Target, current: &str) -> Result<(), BlockError> {
    let (rest, _) = split(current)?;
    target.check(&rest)
}

/// Refuses a file of the same tool that ranks above the binding's own unless it defers to it:
/// it names no default source and holds no block of an earlier binding.
pub fn defers(target: Target, current: &str) -> Result<(), BlockError> {
    refuse(current.contains(BEGIN))?;
    target.check(current)
}

/// Whether the file holds exactly the binding of this base and projection: binding it again would
/// change nothing. An edited block, another port, a setting of the file's own that conflicts, or
/// content after the block is not bound.
pub fn is_bound(target: Target, current: &str, base: &Base, projection: &Projection) -> bool {
    bound(target, Some(current), base, projection).is_ok_and(|updated| updated == current)
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
    const PROJECTION: Projection = Projection {
        gradle: GRADLE,
        npmrc: "/home/u/.config/nds-cache/npmrc",
    };

    fn base() -> Base {
        Base::new(18740).unwrap()
    }

    fn bind(target: Target, current: Option<&str>) -> Result<String, BlockError> {
        bound(target, current, &base(), &PROJECTION)
    }

    #[test]
    fn the_environment_names_only_tools_without_a_configuration_file() {
        let text = bind(Target::Environment, None).unwrap();
        assert!(text.contains("export RUSTUP_DIST_SERVER='http://127.0.0.1:18740/rustup'\n"));
        assert!(text.contains("export NDS_CACHE_BASE='http://127.0.0.1:18740'\n"));
        assert!(
            text.contains("export npm_config_globalconfig='/home/u/.config/nds-cache/npmrc'\n")
        );
        assert_eq!(text.matches("export ").count(), ENVIRONMENT.len() + 1);
        assert!(!text.contains("/npm/") && !text.contains("GOPROXY") && !text.contains("simple"));
        // uv and pub record the index in their lockfiles, so neither is bound.
        assert!(!text.contains("PUB_HOSTED_URL") && !text.contains("/pub'"));
        assert!(Base::new(0).is_none());
        let quoted = Projection {
            npmrc: "/home/o'brien/.config/nds-cache/npmrc",
            ..PROJECTION
        };
        assert_eq!(
            bound(Target::Environment, None, &base(), &quoted),
            Err(BlockError::Invalid)
        );
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
            &PROJECTION,
        )
        .unwrap();
        assert_eq!(moved.matches(BEGIN).count(), 1);
        assert!(moved.contains("127.0.0.1:18741") && !moved.contains("127.0.0.1:18740"));
        assert_eq!(
            unbound(Target::Cargo, &moved, GRADLE).unwrap().as_deref(),
            Some(own)
        );
        // A file the binding created is removed again; one it found empty stays.
        let created = bind(Target::Pip, None).unwrap();
        assert!(created.contains(CREATED));
        assert_eq!(unbound(Target::Pip, &created, GRADLE).unwrap(), None);
        let found_empty = bind(Target::Pip, Some("")).unwrap();
        assert_eq!(
            unbound(Target::Pip, &found_empty, GRADLE)
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
            (Target::Pip, "[global]\ntimeout = 60\n"),
            (Target::Go, "GOPROXY=https://goproxy.example.internal\n"),
            // An included file is never followed, so including one at all is refused.
            (Target::Cargo, "include = [\"other.toml\"]\n"),
            (Target::Cargo, "include = \"other.toml\"\n"),
            (Target::Cargo, "[[include]]\npath = \"other.toml\"\n"),
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
                Target::Pip,
                "[install]\nindex-url = https://pypi.example.internal/simple\n",
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
    fn the_user_npmrc_leaves_the_default_registry_to_the_binding() {
        for foreign in [
            "registry = https://npm.example.internal/\n",
            "Registry=https://npm.example.internal/\n",
            &format!("{BEGIN}\nregistry=http://127.0.0.1:18740/npm/\n{END}\n"),
        ] {
            assert_eq!(
                defers(Target::Npm, foreign),
                Err(BlockError::Conflict),
                "{foreign}"
            );
        }
        // Scoped registries and credentials stay the user's own.
        let own = "@corp:registry=https://npm.example.internal/\n//npm.example.internal/:_authToken=x\n# registry=old\n";
        assert_eq!(defers(Target::Npm, own), Ok(()));
    }

    #[test]
    fn a_lower_file_yields_only_without_a_source_of_its_own() {
        let private = "[source.crates-io]\nreplace-with = \"corp\"\n";
        assert_eq!(yields(Target::Cargo, private), Err(BlockError::Conflict));
        let including = "include = [\"other.toml\"]\n";
        assert_eq!(yields(Target::Cargo, including), Err(BlockError::Conflict));
        assert_eq!(yields(Target::Cargo, "[build]\njobs = 2\n"), Ok(()));
        // A block of an earlier user binding is the module's own.
        let bound = bind(Target::Cargo, Some("[build]\njobs = 2\n")).unwrap();
        assert_eq!(yields(Target::Cargo, &bound), Ok(()));
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
        assert_eq!(
            bind(
                Target::Npm,
                Some("registry=https://npm.example.internal/\n")
            ),
            Err(BlockError::Conflict)
        );
        assert_eq!(unbound(Target::Gradle, GRADLE, GRADLE), Ok(None));
        assert_eq!(
            unbound(Target::Gradle, "// edited\n", GRADLE),
            Err(BlockError::Conflict)
        );
    }

    #[test]
    fn only_the_exact_projection_of_the_base_is_bound() {
        let other = Base::new(18741).unwrap();
        for target in Target::ALL {
            let text = bind(target, None).unwrap();
            assert!(is_bound(target, &text, &base(), &PROJECTION), "{target:?}");
            // The Gradle script reads the base from the environment file.
            let stale = is_bound(target, &text, &other, &PROJECTION);
            assert_eq!(stale, target == Target::Gradle, "{target:?}");
        }
        let cargo = bind(Target::Cargo, Some("[build]\njobs = 2\n")).unwrap();
        let elsewhere = cargo.replace(
            "sparse+http://127.0.0.1:18740/crates-index/",
            "sparse+https://unselected.invalid/index/",
        );
        let edited = |text: &str| {
            text.replace(
                "\nexport NDS_CACHE_BASE",
                "\nexport A='1'\nexport NDS_CACHE_BASE",
            )
        };
        let environment = bind(Target::Environment, None).unwrap();
        let moved = Projection {
            npmrc: "/home/v/.config/nds-cache/npmrc",
            ..PROJECTION
        };
        for (target, text, projection) in [
            (Target::Cargo, elsewhere, PROJECTION),
            (
                Target::Cargo,
                format!("{cargo}[net]\nretry = 1\n"),
                PROJECTION,
            ),
            (Target::Cargo, format!("{cargo}{cargo}"), PROJECTION),
            (
                Target::Cargo,
                format!("include = [\"other.toml\"]\n{cargo}"),
                PROJECTION,
            ),
            (
                Target::Cargo,
                cargo.replace(
                    "[build]\njobs = 2\n",
                    "[source.crates-io]\nreplace-with = \"corp\"\n",
                ),
                PROJECTION,
            ),
            (Target::Environment, edited(&environment), PROJECTION),
            (Target::Environment, environment.clone(), moved),
            (
                Target::Environment,
                format!("{environment}{environment}"),
                PROJECTION,
            ),
            (
                Target::Npm,
                bind(Target::Npm, None).unwrap().replace("/npm/", "/other/"),
                PROJECTION,
            ),
            (
                Target::Go,
                bind(Target::Go, None)
                    .unwrap()
                    .replace("/goproxy", "/goproxy,direct"),
                PROJECTION,
            ),
            (
                Target::Pip,
                bind(Target::Pip, None)
                    .unwrap()
                    .replace("/simple/", "/other/"),
                PROJECTION,
            ),
            (Target::Gradle, format!("{GRADLE}// edited\n"), PROJECTION),
        ] {
            assert!(
                !is_bound(target, &text, &base(), &projection),
                "{target:?}: {text}"
            );
        }
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
            ("registry", "{base}/npm/"),
            ("PIP_INDEX_URL", "{base}/pypi/simple/"),
            ("GOPROXY", "{base}/goproxy"),
        ] {
            assert!(listed(name, value), "{name}");
            let written = value.replace("{base}", &base().url());
            assert!(
                Target::ALL
                    .iter()
                    .any(|target| target.content(&base(), &PROJECTION).contains(&written)),
                "{name}"
            );
        }
    }
}
