//! The client against a real filesystem scope and a real loopback listener.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use nddev_device_sync_cache_client::{
    CancellationToken, Client, Error, NativeIo, NetworkCache, Scope,
    binding::{BEGIN, Base, Target},
};
use std::{
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

fn client(root: &Path, base: Base) -> Client {
    let io = NativeIo::new(1).unwrap().local_handle();
    Client::new(io, Scope::rooted(root).unwrap(), base)
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
    let scope = Scope::rooted(root.path()).unwrap();
    // The user's own configuration that binding has to keep.
    std::fs::create_dir_all(root.path().join(".cargo")).unwrap();
    std::fs::write(scope.path(Target::Cargo), "[build]\njobs = 2\n").unwrap();
    std::fs::write(
        scope.path(Target::Npm),
        "@corp:registry=https://npm.example.internal/\n",
    )
    .unwrap();
    let before = snapshot(root.path());

    let base = ready_cache();
    let client = client(root.path(), base.clone());
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
    let npm = std::fs::read_to_string(scope.path(Target::Npm)).unwrap();
    assert!(npm.starts_with("@corp:registry=") && npm.contains(&format!("registry={url}/npm/")));
    for (target, needle) in [
        (Target::Pip, format!("index-url = {url}/pypi/simple/")),
        (Target::Uv, format!("url = \"{url}/pypi/simple/\"")),
        (Target::Go, format!("GOPROXY={url}/goproxy")),
        (
            Target::Environment,
            format!("export PUB_HOSTED_URL='{url}/pub'"),
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
    for target in [Target::Environment, Target::Pip, Target::Uv, Target::Go] {
        assert!(!scope.path(target).parent().unwrap().exists(), "{target:?}");
    }
    assert!(root.path().join(".cargo").is_dir() && root.path().join(".config").is_dir());
}

#[tokio::test]
async fn a_cache_that_does_not_answer_is_not_bound() {
    let root = tempfile::tempdir().unwrap();
    let client = client(root.path(), absent_cache());
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
    let scope = Scope::rooted(root.path()).unwrap();
    std::fs::create_dir_all(scope.path(Target::Go).parent().unwrap()).unwrap();
    std::fs::write(
        scope.path(Target::Go),
        "GOPROXY=https://goproxy.example.internal\n",
    )
    .unwrap();
    let before = snapshot(root.path());
    let client = client(root.path(), ready_cache());
    assert_eq!(
        client.bind(CancellationToken::new()).await,
        Err(Error::Conflict)
    );
    assert_eq!(snapshot(root.path()), before);
}

#[tokio::test]
async fn links_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let scope = Scope::rooted(root.path()).unwrap();
    let elsewhere = root.path().join("elsewhere");
    std::fs::write(&elsewhere, "").unwrap();
    std::os::unix::fs::symlink(&elsewhere, scope.path(Target::Npm)).unwrap();
    let client = client(root.path(), ready_cache());
    assert_eq!(
        client.bind(CancellationToken::new()).await,
        Err(Error::Conflict)
    );
    assert!(!std::fs::read_to_string(&elsewhere).unwrap().contains(BEGIN));

    let linked_root = root.path().join("linked-home");
    std::os::unix::fs::symlink(root.path(), &linked_root).unwrap();
    assert_eq!(Scope::rooted(&linked_root), Err(Error::Conflict));
    assert!(Scope::rooted(Path::new("relative")).is_err());
}
