//! The client against a real filesystem scope and a real loopback listener.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use nddev_device_sync_cache_client::{
    CancellationToken, Client, Error, NativeIo, NetworkCache, Scope,
    binding::{BEGIN, Base, Target},
};
use std::{
    ffi::OsString,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
};

/// A listener that answers every request like a ready cache.
fn ready_cache() -> Base {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut request = [0u8; 512];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}");
        }
    });
    Base::new(port).unwrap()
}

/// A port nothing listens on.
fn absent_cache() -> Base {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    Base::new(port).unwrap()
}

/// The scope of a lane whose jobs run with `HOME` at the root and the given other variables.
fn lane(root: &Path, variables: &[(&str, &Path)]) -> Result<Scope, Error> {
    Scope::lane_in(root, |name| {
        variables
            .iter()
            .chain(&[("HOME", root)])
            .find(|(variable, _)| *variable == name)
            .map(|(_, value)| OsString::from(value))
    })
}

fn client(scope: Scope, base: Base) -> Client {
    let io = NativeIo::new(1).unwrap().local_handle();
    Client::new(io, scope, base)
}

fn snapshot(root: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let name = path.strip_prefix(root).unwrap().display().to_string();
                files.push((name, std::fs::read_to_string(&path).unwrap()));
            }
        }
    }
    files.sort();
    files
}

#[tokio::test]
async fn binding_writes_every_file_and_unbinding_restores_the_scope() {
    let root = tempfile::tempdir().unwrap();
    let scope = lane(root.path(), &[]).unwrap();
    // The user's own configuration that binding has to keep.
    std::fs::create_dir_all(root.path().join(".cargo")).unwrap();
    std::fs::write(scope.path(Target::Cargo), "[build]\njobs = 2\n").unwrap();
    let npm_user = root.path().join(".npmrc");
    std::fs::write(&npm_user, "@corp:registry=https://npm.example.internal/\n").unwrap();
    let before = snapshot(root.path());

    let base = ready_cache();
    let client = client(scope.clone(), base.clone());
    let status = client.bind(CancellationToken::new()).await.unwrap();
    assert!(status.ready);
    assert!(
        status.bindings.iter().all(|binding| binding.bound),
        "{status:?}"
    );
    let url = base.url();
    let cargo = std::fs::read_to_string(scope.path(Target::Cargo)).unwrap();
    assert!(
        cargo.starts_with("[build]\njobs = 2\n")
            && cargo.contains(&format!("sparse+{url}/crates-index/"))
    );
    // npm gets its own global file, which the environment names; Bun reads neither.
    assert_eq!(
        std::fs::read_to_string(&npm_user).unwrap(),
        "@corp:registry=https://npm.example.internal/\n"
    );
    let npmrc = scope.path(Target::Npm);
    for (target, needle) in [
        (Target::Npm, format!("registry={url}/npm/")),
        (
            Target::Environment,
            format!("export npm_config_globalconfig='{}'", npmrc.display()),
        ),
        (Target::Pip, format!("index-url = {url}/pypi/simple/")),
        (Target::Go, format!("GOPROXY={url}/goproxy")),
        (
            Target::Environment,
            format!("export RUSTUP_DIST_SERVER='{url}/rustup'"),
        ),
        (Target::Gradle, "NDS_CACHE_BASE".to_owned()),
    ] {
        let text = std::fs::read_to_string(scope.path(target)).unwrap();
        assert!(text.contains(&needle), "{target:?}: {text}");
    }
    // Binding twice changes nothing.
    let bound = snapshot(root.path());
    client.bind(CancellationToken::new()).await.unwrap();
    assert_eq!(snapshot(root.path()), bound);

    let status = client.unbind(CancellationToken::new()).await.unwrap();
    assert!(status.bindings.iter().all(|binding| !binding.bound));
    assert_eq!(snapshot(root.path()), before);
    for target in [Target::Environment, Target::Npm, Target::Pip, Target::Go] {
        assert!(!scope.path(target).parent().unwrap().exists(), "{target:?}");
    }
    assert!(root.path().join(".cargo").is_dir() && root.path().join(".config").is_dir());
}

/// The kinds the status reports as bound.
fn bound(status: &nddev_device_sync_cache_client::view::Status) -> Vec<&'static str> {
    status
        .bindings
        .iter()
        .filter(|binding| binding.bound)
        .map(|binding| binding.kind)
        .collect()
}

#[tokio::test]
async fn only_the_bound_projection_in_force_is_bound() {
    let work = tempfile::tempdir().unwrap();
    let (root, shared) = (work.path().join("lane"), work.path().join("shared/cargo"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    let scope = lane(&root, &[("CARGO_HOME", &shared)]).unwrap();
    let base = ready_cache();
    let client = client(scope.clone(), base.clone());
    client.bind(CancellationToken::new()).await.unwrap();
    let all = ["environment", "cargo", "npm", "pip", "go", "gradle"];
    let status = client.status(CancellationToken::new()).await.unwrap();
    assert_eq!(bound(&status), all);

    // A binding of another port is stale for this one, except the script that reads the base.
    let other = self::client(scope.clone(), ready_cache());
    let status = other.status(CancellationToken::new()).await.unwrap();
    assert_eq!(bound(&status), ["gradle"]);

    // A block that still carries the marker but names another source is not the binding.
    let cargo = scope.path(Target::Cargo);
    let written = std::fs::read_to_string(&cargo).unwrap();
    let elsewhere = written.replace(
        &format!("sparse+{}/crates-index/", base.url()),
        "sparse+https://unselected.invalid/index/",
    );
    assert!(elsewhere.contains(BEGIN) && elsewhere != written);
    std::fs::write(&cargo, &elsewhere).unwrap();
    let status = client.status(CancellationToken::new()).await.unwrap();
    assert!(
        status.ready && !bound(&status).contains(&"cargo"),
        "{status:?}"
    );
    client.bind(CancellationToken::new()).await.unwrap();
    assert_eq!(std::fs::read_to_string(&cargo).unwrap(), written);

    // A source in the jobs' Cargo home and a default registry in the user's npmrc each take that
    // tool out of the binding, as they would refuse binding it.
    std::fs::write(
        shared.join("config.toml"),
        "[source.crates-io]\nreplace-with = \"corp\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join(".npmrc"),
        "registry=https://npm.example.internal/\n",
    )
    .unwrap();
    let status = client.status(CancellationToken::new()).await.unwrap();
    assert_eq!(bound(&status), ["environment", "pip", "go", "gradle"]);
    std::fs::remove_file(shared.join("config.toml")).unwrap();
    std::fs::remove_file(root.join(".npmrc")).unwrap();
    let status = client.status(CancellationToken::new()).await.unwrap();
    assert_eq!(bound(&status), all);
    client.unbind(CancellationToken::new()).await.unwrap();
    assert!(snapshot(&root).is_empty());
}

/// What Cargo reads in place of the binding's file, or follows outside every checked file.
const SHADOWS: [(&str, &str); 2] = [
    (
        "config",
        "[source.crates-io]\nreplace-with = \"other\"\n[source.other]\nregistry = \"sparse+https://unselected.invalid/index/\"\n",
    ),
    ("config.toml", "include = [\"other.toml\"]\n"),
];

#[tokio::test]
async fn a_cargo_file_the_binding_cannot_check_refuses_it_and_unbinds_cargo() {
    let work = tempfile::tempdir().unwrap();
    let (root, shared) = (work.path().join("lane"), work.path().join("shared/cargo"));
    std::fs::create_dir_all(root.join(".cargo")).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    let scope = lane(&root, &[("CARGO_HOME", &shared)]).unwrap();
    let client = client(scope.clone(), ready_cache());
    // In the lane's own Cargo directory and in the jobs' Cargo home alike.
    for directory in [root.join(".cargo"), shared.clone()] {
        for (name, text) in SHADOWS {
            let path = directory.join(name);
            std::fs::write(&path, text).unwrap();
            let before = (snapshot(&root), snapshot(&shared));
            assert_eq!(
                client.bind(CancellationToken::new()).await,
                Err(Error::Conflict),
                "{}",
                path.display()
            );
            assert_eq!((snapshot(&root), snapshot(&shared)), before);
            std::fs::remove_file(&path).unwrap();
        }
    }
    // A bound lane whose Cargo file is shadowed or includes one later no longer binds Cargo.
    client.bind(CancellationToken::new()).await.unwrap();
    let cargo = std::fs::read_to_string(scope.path(Target::Cargo)).unwrap();
    for directory in [root.join(".cargo"), shared.clone()] {
        for (name, text) in SHADOWS {
            let path = directory.join(name);
            if name == "config.toml" && directory == root.join(".cargo") {
                std::fs::write(&path, format!("{text}{cargo}")).unwrap();
            } else {
                std::fs::write(&path, text).unwrap();
            }
            let status = client.status(CancellationToken::new()).await.unwrap();
            assert_eq!(
                bound(&status),
                ["environment", "npm", "pip", "go", "gradle"],
                "{}",
                path.display()
            );
            if name == "config.toml" && directory == root.join(".cargo") {
                std::fs::write(&path, &cargo).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
        }
    }
    let status = client.status(CancellationToken::new()).await.unwrap();
    assert!(status.bindings.iter().all(|binding| binding.bound));
    client.unbind(CancellationToken::new()).await.unwrap();
    assert!(snapshot(&root).is_empty());
}

/// The `nds-cache` command for the user's own scope at `home`, with only these variables.
fn user(home: &Path, command: &str, base: &Base) -> (Option<i32>, serde_json::Value) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nds-cache"))
        .args([command, "--port", &base.port().to_string()])
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let report = serde_json::from_slice(&output.stdout).unwrap();
    (output.status.code(), report)
}

#[test]
fn a_user_cargo_home_the_binding_cannot_check_refuses_it_and_unbinds_cargo() {
    let home = tempfile::tempdir().unwrap();
    let cargo = home.path().join(".cargo");
    std::fs::create_dir_all(&cargo).unwrap();
    let base = ready_cache();
    for (name, text) in SHADOWS {
        std::fs::write(cargo.join(name), text).unwrap();
        let before = snapshot(home.path());
        let (code, report) = user(home.path(), "bind", &base);
        assert_eq!(
            (code, report["error"].as_str()),
            (Some(2), Some("conflict"))
        );
        assert_eq!(snapshot(home.path()), before);
        std::fs::remove_file(cargo.join(name)).unwrap();
    }
    assert_eq!(user(home.path(), "bind", &base).0, Some(0));
    let written = std::fs::read_to_string(cargo.join("config.toml")).unwrap();
    for (name, text) in SHADOWS {
        let path = cargo.join(name);
        let shadowing = if name == "config" {
            text.to_owned()
        } else {
            format!("{text}{written}")
        };
        std::fs::write(&path, shadowing).unwrap();
        let (code, report) = user(home.path(), "status", &base);
        let cargo_bound = report["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|binding| binding["kind"] == "cargo")
            .unwrap()["bound"]
            .clone();
        assert_eq!(
            (code, cargo_bound),
            (Some(0), serde_json::Value::Bool(false)),
            "{name}"
        );
        if name == "config" {
            std::fs::remove_file(&path).unwrap();
        } else {
            std::fs::write(&path, &written).unwrap();
        }
    }
    let (_, report) = user(home.path(), "status", &base);
    assert!(
        report["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|binding| binding["bound"] == true)
    );
    assert_eq!(user(home.path(), "unbind", &base).0, Some(0));
    assert_eq!(snapshot(home.path()), Vec::<(String, String)>::new());
}

#[tokio::test]
async fn a_cache_that_does_not_answer_is_not_bound() {
    let root = tempfile::tempdir().unwrap();
    let client = client(lane(root.path(), &[]).unwrap(), absent_cache());
    assert_eq!(
        client.bind(CancellationToken::new()).await,
        Err(Error::NotReady)
    );
    assert!(snapshot(root.path()).is_empty());
    let status = client.status(CancellationToken::new()).await.unwrap();
    assert!(!status.ready && status.bindings.iter().all(|binding| !binding.bound));
}

#[tokio::test]
async fn a_registry_of_the_user_refuses_the_whole_binding() {
    let root = tempfile::tempdir().unwrap();
    let scope = lane(root.path(), &[]).unwrap();
    std::fs::create_dir_all(scope.path(Target::Go).parent().unwrap()).unwrap();
    std::fs::write(
        scope.path(Target::Go),
        "GOPROXY=https://goproxy.example.internal\n",
    )
    .unwrap();
    let before = snapshot(root.path());
    let client = client(scope, ready_cache());
    assert_eq!(
        client.bind(CancellationToken::new()).await,
        Err(Error::Conflict)
    );
    assert_eq!(snapshot(root.path()), before);
}

#[tokio::test]
async fn a_default_registry_in_the_user_npmrc_refuses_the_binding() {
    let root = tempfile::tempdir().unwrap();
    let scope = lane(root.path(), &[]).unwrap();
    std::fs::write(
        root.path().join(".npmrc"),
        "registry=https://npm.example.internal/\n",
    )
    .unwrap();
    let before = snapshot(root.path());
    let client = client(scope, ready_cache());
    assert_eq!(
        client.bind(CancellationToken::new()).await,
        Err(Error::Conflict)
    );
    assert_eq!(snapshot(root.path()), before);
}

#[tokio::test]
async fn a_lane_keeps_the_cargo_home_of_its_jobs() {
    let work = tempfile::tempdir().unwrap();
    let (root, shared) = (work.path().join("lane"), work.path().join("shared/cargo"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    let scope = lane(&root, &[("CARGO_HOME", &shared)]).unwrap();
    assert_eq!(scope.path(Target::Cargo), root.join(".cargo/config.toml"));

    // A source of the jobs' Cargo home would be shadowed by the lane's file, so it refuses.
    let private = "[source.crates-io]\nreplace-with = \"corp\"\n";
    std::fs::write(shared.join("config.toml"), private).unwrap();
    let client = client(scope, ready_cache());
    assert_eq!(
        client.bind(CancellationToken::new()).await,
        Err(Error::Conflict)
    );
    assert!(snapshot(&root).is_empty());

    // Other settings there stay as they are, and evicting the store keeps the binding.
    std::fs::write(shared.join("config.toml"), "[net]\nretry = 1\n").unwrap();
    client.bind(CancellationToken::new()).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(shared.join("config.toml")).unwrap(),
        "[net]\nretry = 1\n"
    );
    std::fs::remove_dir_all(&shared).unwrap();
    let status = client.status(CancellationToken::new()).await.unwrap();
    assert!(status.bindings.iter().all(|binding| binding.bound));
    client.unbind(CancellationToken::new()).await.unwrap();
    assert!(snapshot(&root).is_empty());
}

#[test]
fn a_lane_needs_the_environment_of_its_jobs() {
    let root = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let (defaults, go_env) = (root.path().join(".config"), elsewhere.path().join("env"));
    assert!(lane(root.path(), &[("XDG_CONFIG_HOME", &defaults)]).is_ok());
    for variables in [
        [("XDG_CONFIG_HOME", elsewhere.path())],
        [("GOENV", go_env.as_path())],
    ] {
        let variables = &variables[..];
        assert_eq!(lane(root.path(), variables), Err(Error::Invalid));
    }
    // Jobs whose HOME is not the lane would never read its files.
    let other = Scope::lane_in(root.path(), |name| {
        (name == "HOME").then(|| elsewhere.path().into())
    });
    assert_eq!(other, Err(Error::Invalid));
    assert!(lane(root.path(), &[("CARGO_HOME", Path::new("relative"))]).is_err());
}

#[tokio::test]
async fn links_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let scope = lane(root.path(), &[]).unwrap();
    let elsewhere = root.path().join("elsewhere");
    std::fs::write(&elsewhere, "").unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.path().join(".npmrc")).unwrap();
    let client = client(scope, ready_cache());
    assert_eq!(
        client.bind(CancellationToken::new()).await,
        Err(Error::Conflict)
    );
    assert!(!std::fs::read_to_string(&elsewhere).unwrap().contains(BEGIN));

    let linked_root = root.path().join("linked-home");
    std::os::unix::fs::symlink(root.path(), &linked_root).unwrap();
    assert_eq!(lane(&linked_root, &[]), Err(Error::Conflict));
    assert!(lane(Path::new("relative"), &[]).is_err());
}

#[tokio::test]
async fn a_file_past_the_limit_or_one_the_binding_would_grow_past_it_is_refused() {
    const LIMIT: usize = 1 << 20;
    let root = tempfile::tempdir().unwrap();
    let cargo = root.path().join(".cargo").join("config.toml");
    std::fs::create_dir_all(cargo.parent().unwrap()).unwrap();
    let client = client(lane(root.path(), &[]).unwrap(), ready_cache());
    // One file the client could not read back, and one the binding's block would push past.
    for size in [LIMIT + 1, LIMIT - 8] {
        std::fs::write(&cargo, format!("{}\n", "#".repeat(size - 1))).unwrap();
        let before = snapshot(root.path());
        assert_eq!(
            client.bind(CancellationToken::new()).await,
            Err(Error::Invalid)
        );
        assert_eq!(snapshot(root.path()), before);
    }
    std::fs::write(&cargo, format!("{}\n", "#".repeat(LIMIT))).unwrap();
    assert_eq!(
        client.unbind(CancellationToken::new()).await,
        Err(Error::Invalid)
    );
}

#[tokio::test]
async fn a_scope_another_binder_holds_answers_busy_and_stays_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let held = std::fs::File::open(root.path()).unwrap();
    rustix::fs::flock(&held, rustix::fs::FlockOperation::LockExclusive).unwrap();
    let client = client(lane(root.path(), &[]).unwrap(), ready_cache());
    assert_eq!(
        client.bind(CancellationToken::new()).await,
        Err(Error::Busy)
    );
    assert!(snapshot(root.path()).is_empty());
    drop(held);
    assert!(client.bind(CancellationToken::new()).await.is_ok());
}

#[tokio::test]
async fn binds_of_one_scope_run_one_at_a_time() {
    let root = tempfile::tempdir().unwrap();
    let base = ready_cache();
    let scope = lane(root.path(), &[]).unwrap();
    let (first, second) = (
        client(scope.clone(), base.clone()),
        client(scope.clone(), base),
    );
    let (one, two) = tokio::join!(
        first.bind(CancellationToken::new()),
        second.bind(CancellationToken::new())
    );
    assert!(one.is_ok() && two.is_ok(), "{one:?} {two:?}");
    let cargo = std::fs::read_to_string(scope.path(Target::Cargo)).unwrap();
    assert_eq!(cargo.matches(BEGIN).count(), 1);
    assert!(
        snapshot(root.path())
            .iter()
            .all(|(name, _)| !name.contains("nds-cache.") || !name.ends_with(".tmp")),
        "no temporary file is left"
    );
    first.unbind(CancellationToken::new()).await.unwrap();
    assert!(snapshot(root.path()).is_empty());
}
