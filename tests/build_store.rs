//! Real HTTP and filesystem acceptance for the isolated compiled-result store.
use axum::{
    body::Body,
    http::{HeaderMap, Method, Uri},
};
use nddev_device_sync_cache::{
    builds::{self, BuildStore},
    config, domain, store, telemetry,
};
use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::Duration};

const READER: &str = "Bearer synthetic-device-reader";
const WRITER: &str = "Bearer synthetic-trusted-ci-writer";
fn object() -> String {
    format!("sccache/{}", "a".repeat(64))
}
struct Fixture {
    _temp: tempfile::TempDir,
    settings: builds::Settings,
    downloads: PathBuf,
    builds: Arc<BuildStore>,
    base: String,
    app: Arc<nddev_device_sync_cache::http::App>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let downloads = temp.path().join("downloads");
        let credentials = temp.path().join("credentials.json");
        std::fs::write(
            &credentials,
            json!({"schema":1,
            "readers":[builds::credential_hash(READER)],
            "ci_writers":[builds::credential_hash(WRITER)]})
            .to_string(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let settings = builds::Settings {
            root: temp.path().join("builds"),
            credentials,
            capacity: 1 << 30,
            max_object: 1 << 20,
            retention: Duration::from_secs(86400),
        };
        let mut config = config::Settings::read(|name| match name {
            "NDS_CACHE_ROOT" => Some(downloads.to_string_lossy().into_owned()),
            _ => None,
        })
        .unwrap();
        config.builds = Some(settings.clone());
        let app = nddev_device_sync_cache::app(
            &config,
            telemetry::Identity::read(&|_| None).unwrap(),
            domain::Routes::production(),
        )
        .await
        .unwrap();
        let builds = app.builds.as_ref().unwrap().clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = nddev_device_sync_cache::http::router(app.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            _temp: temp,
            settings,
            downloads,
            builds,
            base,
            app,
            task,
        }
    }
    fn url(&self, key: &str) -> String {
        format!("{}/v1/build-cache/{key}", self.base)
    }
    async fn put(&self, key: &str, auth: &str, body: &str) -> u16 {
        reqwest::Client::new()
            .put(self.url(key))
            .header("Authorization", auth)
            .body(body.to_owned())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
}
#[tokio::test]
async fn authenticated_native_http_objects_are_immutable_and_separate() {
    let f = Fixture::new().await;
    let key = object();
    let client = reqwest::Client::new();
    assert_eq!(client.get(f.url(&key)).send().await.unwrap().status(), 403);
    // OpenDAL's real startup probe reads an absent non-object sentinel.
    assert_eq!(
        client
            .get(f.url("sccache/.sccache_check"))
            .header("Authorization", READER)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        f.put("sccache/.sccache_check", READER, "Hello, World!")
            .await,
        403
    );
    assert_eq!(
        f.put("sccache/.sccache_check", WRITER, "Hello, World!")
            .await,
        201
    );
    assert_eq!(
        f.put("sccache/.sccache_check", WRITER, "Hello, World!")
            .await,
        204
    );
    assert_eq!(f.put(&key, READER, "compiled").await, 403);
    assert_eq!(f.put(&key, WRITER, "compiled").await, 201);
    assert_eq!(f.put(&key, WRITER, "compiled").await, 204);
    assert_eq!(f.put(&key, WRITER, "conflict").await, 409);
    let response = client
        .get(f.url(&key))
        .header("Authorization", READER)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-length"], "8");
    assert_eq!(response.bytes().await.unwrap(), "compiled");
    let head = client
        .head(f.url(&key))
        .header("Authorization", READER)
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["content-length"], "8");
    assert!(head.bytes().await.unwrap().is_empty());
    assert_eq!(f.app.cache.store().usage().objects, 0);
    for key in [
        "sccache/no-hash",
        "arbitrary/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "gradle/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?token=secret",
        "sccache/%2e%2e/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        assert!(
            !client
                .get(f.url(key))
                .header("Authorization", READER)
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
    }
    assert_eq!(
        client
            .get(f.url(&key))
            .header("Authorization", READER)
            .header("Cookie", "secret")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        f.put(
            &format!("gradle/{}", "b".repeat(64)),
            WRITER,
            &"x".repeat((1 << 20) + 1)
        )
        .await,
        413
    );
    assert_eq!(
        client
            .delete(f.url(&key))
            .header("Authorization", WRITER)
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
}
#[tokio::test]
async fn body_lifetime_lease_prevents_collection_and_repeat_is_idempotent() {
    let f = Fixture::new().await;
    let key = object();
    assert_eq!(f.put(&key, WRITER, "compiled").await, 201);
    let uri: Uri = f.url(&key).parse().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("authorization", READER.parse().unwrap());
    let response = f
        .builds
        .respond(&Method::GET, &uri, &headers, Body::empty())
        .await;
    assert_eq!(response.status(), 200);
    let later = store::now() + 86401;
    assert!(f.builds.collect(later).await.unwrap().busy);
    drop(response);
    let removed = f.builds.collect(later).await.unwrap();
    assert_eq!(removed.removed_objects, 1);
    assert_eq!(removed.removed_bytes, 8);
    assert_eq!(removed.failed_objects, 0);
    assert_eq!(f.builds.collect(later).await.unwrap().removed_objects, 0);
}
#[tokio::test]
async fn observed_hits_survive_reopen_and_future_or_unknown_data_stays() {
    let f = Fixture::new().await;
    let key = object();
    assert_eq!(f.put(&key, WRITER, "compiled").await, 201);
    let future = store::now() + 200000;
    let meta = walk_meta(&f.settings.root).pop().unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&meta).unwrap()).unwrap();
    value["last_access"] = json!(future);
    std::fs::write(&meta, value.to_string()).unwrap();
    let unknown = f.settings.root.join("meta/aa/future-format.json");
    std::fs::create_dir_all(unknown.parent().unwrap()).unwrap();
    std::fs::write(&unknown, "{\"schema\":99}").unwrap();
    let reopened = Arc::new(BuildStore::open(&f.settings, &f.downloads).await.unwrap());
    assert_eq!(
        reopened
            .collect(store::now())
            .await
            .unwrap()
            .removed_objects,
        0
    );
    let uri: Uri = f.url(&key).parse().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("authorization", READER.parse().unwrap());
    let response = reopened
        .respond(&Method::GET, &uri, &headers, Body::empty())
        .await;
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 100)
            .await
            .unwrap(),
        "compiled"
    );
    let persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&meta).unwrap()).unwrap();
    assert_eq!(persisted["last_access"], future);
    assert_eq!(
        std::fs::read_to_string(&unknown).unwrap(),
        "{\"schema\":99}"
    );
    // An old completed entry becomes live on a real read, including after reopening the store.
    value["last_access"] = json!(store::now() - 86401);
    std::fs::write(&meta, value.to_string()).unwrap();
    let reopened = Arc::new(BuildStore::open(&f.settings, &f.downloads).await.unwrap());
    let response = reopened
        .respond(&Method::GET, &uri, &headers, Body::empty())
        .await;
    axum::body::to_bytes(response.into_body(), 100)
        .await
        .unwrap();
    let again = BuildStore::open(&f.settings, &f.downloads).await.unwrap();
    assert_eq!(
        again.collect(store::now()).await.unwrap().removed_objects,
        0
    );
    assert!(unknown.exists());
}
fn walk_meta(root: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(root.join("meta"))
        .unwrap()
        .flat_map(|d| {
            std::fs::read_dir(d.unwrap().path())
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect::<Vec<_>>()
        })
        .collect()
}
#[tokio::test]
async fn truncated_uploads_leave_no_committed_or_partial_object() {
    use tokio::io::AsyncWriteExt;
    let f = Fixture::new().await;
    let mut stream = tokio::net::TcpStream::connect(f.base.strip_prefix("http://").unwrap())
        .await
        .unwrap();
    stream.write_all(format!("PUT /v1/build-cache/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: {WRITER}\r\nContent-Length: 100\r\n\r\nshort",object()).as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
    drop(stream);
    for _ in 0..100 {
        if std::fs::read_dir(f.settings.root.join("tmp"))
            .unwrap()
            .next()
            .is_none()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(f.builds.health()["objects"], 0);
    assert!(
        std::fs::read_dir(f.settings.root.join("tmp"))
            .unwrap()
            .next()
            .is_none()
    );
}
#[tokio::test]
async fn store_aliases_unowned_roots_and_exposed_credentials_are_refused() {
    let f = Fixture::new().await;
    let mut bad = f.settings.clone();
    bad.root = f.downloads.clone();
    assert!(BuildStore::open(&bad, &f.downloads).await.is_err());
    bad.root = f._temp.path().join("unowned");
    std::fs::create_dir(&bad.root).unwrap();
    std::fs::write(bad.root.join("keep"), "foreign").unwrap();
    assert!(BuildStore::open(&bad, &f.downloads).await.is_err());
    assert_eq!(
        std::fs::read_to_string(bad.root.join("keep")).unwrap(),
        "foreign"
    );
    let public_copy = f._temp.path().join("other-downloads");
    let _public = store::Store::open(&public_copy, 1 << 30, 1 << 20)
        .await
        .unwrap();
    bad.root = public_copy;
    assert!(BuildStore::open(&bad, &f.downloads).await.is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            &f.settings.credentials,
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(BuildStore::open(&f.settings, &f.downloads).await.is_err());
    }
}
