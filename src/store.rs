//! Object store on the cache volume. Objects become visible only through an atomic
//! rename after a complete, size-bounded write; the volume holds acceleration state only.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{fs, io::AsyncWriteExt};

const MARKER: &str = ".nds-cache-store";
const MARKER_CONTENT: &str = "{\"schema\":1,\"owner\":\"nddev-device-sync-cache\"}\n";

static PENDING: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StoreError {
    #[error("store_root_unowned")]
    Unowned,
    #[error("store_io")]
    Io,
    #[error("store_object_too_large")]
    TooLarge,
}

impl From<std::io::Error> for StoreError {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}

/// Persisted object metadata. Validators come from the upstream response.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Meta {
    pub key: String,
    pub size: u64,
    pub sha256: String,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub fetched_at: u64,
    pub last_access: u64,
}

/// Validators and media type of a completed upstream response.
#[derive(Clone, Debug, Default)]
pub struct Validators {
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub objects: usize,
    pub bytes: u64,
    pub capacity: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Eviction {
    pub objects: usize,
    pub bytes: u64,
}

struct Index {
    entries: HashMap<String, Meta>,
    bytes: u64,
}

pub struct Store {
    root: PathBuf,
    capacity: u64,
    max_object: u64,
    index: Mutex<Index>,
}

/// An in-progress download. Dropping it without `commit` removes the partial file.
pub struct Pending {
    path: PathBuf,
    file: Option<fs::File>,
    hasher: Sha256,
    size: u64,
    limit: u64,
}

impl Pending {
    pub async fn write(&mut self, chunk: &[u8]) -> Result<(), StoreError> {
        self.size += chunk.len() as u64;
        if self.size > self.limit {
            return Err(StoreError::TooLarge);
        }
        self.hasher.update(chunk);
        self.file
            .as_mut()
            .ok_or(StoreError::Io)?
            .write_all(chunk)
            .await?;
        Ok(())
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if self.file.is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .unwrap_or(0)
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Store {
    /// Opens or initializes the store. An existing non-empty root without the ownership
    /// marker is refused rather than adopted. Partial downloads from a previous run are removed.
    pub async fn open(
        root: &Path,
        capacity: u64,
        max_object: u64,
    ) -> Result<(Self, usize), StoreError> {
        fs::create_dir_all(root).await?;
        let marker = root.join(MARKER);
        match fs::read_to_string(&marker).await {
            Ok(content) if content == MARKER_CONTENT => {}
            Ok(_) => return Err(StoreError::Unowned),
            Err(_) => {
                let mut entries = fs::read_dir(root).await?;
                if entries.next_entry().await?.is_some() {
                    return Err(StoreError::Unowned);
                }
                fs::write(&marker, MARKER_CONTENT).await?;
            }
        }
        for directory in ["objects", "meta"] {
            fs::create_dir_all(root.join(directory)).await?;
        }
        let _ = fs::remove_dir_all(root.join("tmp")).await;
        fs::create_dir_all(root.join("tmp")).await?;

        let (entries, invalid) = Self::load(root).await?;
        let bytes = entries.values().map(|meta| meta.size).sum();
        Ok((
            Self {
                root: root.to_owned(),
                capacity,
                max_object,
                index: Mutex::new(Index { entries, bytes }),
            },
            invalid,
        ))
    }

    async fn load(root: &Path) -> Result<(HashMap<String, Meta>, usize), StoreError> {
        let mut entries = HashMap::new();
        let mut invalid = 0;
        let mut shards = fs::read_dir(root.join("meta")).await?;
        while let Some(shard) = shards.next_entry().await? {
            let mut files = fs::read_dir(shard.path()).await?;
            while let Some(file) = files.next_entry().await? {
                let path = file.path();
                let meta = fs::read(&path)
                    .await
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Meta>(&bytes).ok());
                let Some(meta) = meta else {
                    invalid += 1;
                    let _ = fs::remove_file(&path).await;
                    continue;
                };
                let hash = digest(meta.key.as_bytes());
                let object = Self::object_path(root, &hash);
                match fs::metadata(&object).await {
                    Ok(stat) if stat.len() == meta.size => {
                        entries.insert(hash, meta);
                    }
                    _ => {
                        invalid += 1;
                        let _ = fs::remove_file(&path).await;
                        let _ = fs::remove_file(&object).await;
                    }
                }
            }
        }
        Ok((entries, invalid))
    }

    fn object_path(root: &Path, hash: &str) -> PathBuf {
        root.join("objects").join(&hash[..2]).join(hash)
    }

    fn meta_path(root: &Path, hash: &str) -> PathBuf {
        root.join("meta")
            .join(&hash[..2])
            .join(format!("{hash}.json"))
    }

    /// The stored object and its path, recording the access for eviction order.
    pub fn get(&self, key: &str) -> Option<(Meta, PathBuf)> {
        let hash = digest(key.as_bytes());
        let mut index = self.index.lock().ok()?;
        let meta = index.entries.get_mut(&hash)?;
        meta.last_access = now();
        Some((meta.clone(), Self::object_path(&self.root, &hash)))
    }

    pub async fn begin(&self) -> Result<Pending, StoreError> {
        let sequence = PENDING.fetch_add(1, Ordering::Relaxed);
        let name = format!("{:08x}-{sequence:016x}", std::process::id());
        let path = self.root.join("tmp").join(name);
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await?;
        Ok(Pending {
            path,
            file: Some(file),
            hasher: Sha256::new(),
            size: 0,
            limit: self.max_object,
        })
    }

    /// Makes a complete download visible, then evicts least recently used objects until the
    /// store fits its capacity again.
    pub async fn commit(
        &self,
        key: &str,
        mut pending: Pending,
        validators: Validators,
    ) -> Result<(Meta, PathBuf, Eviction), StoreError> {
        let mut file = pending.file.take().ok_or(StoreError::Io)?;
        let flushed = async {
            file.flush().await?;
            file.sync_data().await
        }
        .await;
        drop(file);
        if flushed.is_err() {
            let _ = fs::remove_file(&pending.path).await;
            return Err(StoreError::Io);
        }
        let hash = digest(key.as_bytes());
        let object = Self::object_path(&self.root, &hash);
        let meta_path = Self::meta_path(&self.root, &hash);
        let time = now();
        let meta = Meta {
            key: key.to_owned(),
            size: pending.size,
            sha256: pending
                .hasher
                .clone()
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            content_type: validators.content_type,
            etag: validators.etag,
            last_modified: validators.last_modified,
            fetched_at: time,
            last_access: time,
        };
        let placed = async {
            fs::create_dir_all(object.parent().ok_or(StoreError::Io)?).await?;
            fs::create_dir_all(meta_path.parent().ok_or(StoreError::Io)?).await?;
            fs::rename(&pending.path, &object).await?;
            self.write_meta(&meta_path, &meta).await
        }
        .await;
        if let Err(error) = placed {
            let _ = fs::remove_file(&pending.path).await;
            return Err(error);
        }
        {
            let mut index = self.index.lock().map_err(|_| StoreError::Io)?;
            let previous = index
                .entries
                .insert(hash.clone(), meta.clone())
                .map(|old| old.size)
                .unwrap_or(0);
            index.bytes = index.bytes - previous + meta.size;
        }
        let eviction = self.evict(&hash).await;
        Ok((meta, object, eviction))
    }

    /// Records a successful revalidation of a stored object.
    pub async fn refresh(&self, key: &str, validators: Validators) -> Result<Meta, StoreError> {
        let hash = digest(key.as_bytes());
        let meta = {
            let mut index = self.index.lock().map_err(|_| StoreError::Io)?;
            let meta = index.entries.get_mut(&hash).ok_or(StoreError::Io)?;
            meta.fetched_at = now();
            if validators.etag.is_some() {
                meta.etag = validators.etag;
            }
            if validators.last_modified.is_some() {
                meta.last_modified = validators.last_modified;
            }
            meta.clone()
        };
        self.write_meta(&Self::meta_path(&self.root, &hash), &meta)
            .await?;
        Ok(meta)
    }

    async fn write_meta(&self, path: &Path, meta: &Meta) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(meta).map_err(|_| StoreError::Io)?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, bytes).await?;
        fs::rename(&temporary, path).await?;
        Ok(())
    }

    async fn evict(&self, keep: &str) -> Eviction {
        let mut eviction = Eviction::default();
        loop {
            let victim = {
                let Ok(mut index) = self.index.lock() else {
                    return eviction;
                };
                if index.bytes <= self.capacity {
                    return eviction;
                }
                let Some(hash) = index
                    .entries
                    .iter()
                    .filter(|(hash, _)| hash.as_str() != keep)
                    .min_by_key(|(_, meta)| meta.last_access)
                    .map(|(hash, _)| hash.clone())
                else {
                    return eviction;
                };
                let meta = index.entries.remove(&hash);
                let size = meta.map(|meta| meta.size).unwrap_or(0);
                index.bytes -= size;
                (hash, size)
            };
            let _ = fs::remove_file(Self::object_path(&self.root, &victim.0)).await;
            let _ = fs::remove_file(Self::meta_path(&self.root, &victim.0)).await;
            eviction.objects += 1;
            eviction.bytes += victim.1;
        }
    }

    pub fn usage(&self) -> Usage {
        let index = self.index.lock();
        let (objects, bytes) = index
            .map(|index| (index.entries.len(), index.bytes))
            .unwrap_or((0, 0));
        Usage {
            objects,
            bytes,
            capacity: self.capacity,
        }
    }

    /// Writable and with free filesystem space for at least one maximum-size object.
    pub async fn ready(&self) -> bool {
        let probe = self.root.join("tmp").join(".ready");
        let writable =
            fs::write(&probe, b"ok").await.is_ok() && fs::remove_file(&probe).await.is_ok();
        writable
            && self
                .free_bytes()
                .is_some_and(|free| free >= self.max_object)
    }

    pub fn free_bytes(&self) -> Option<u64> {
        let stat = rustix::fs::statvfs(&self.root).ok()?;
        Some(stat.f_bavail.saturating_mul(stat.f_frsize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store(capacity: u64, max_object: u64) -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let (store, invalid) = Store::open(&root.path().join("store"), capacity, max_object)
            .await
            .unwrap();
        assert_eq!(invalid, 0);
        (root, store)
    }

    async fn put(store: &Store, key: &str, body: &[u8]) -> Eviction {
        let mut pending = store.begin().await.unwrap();
        pending.write(body).await.unwrap();
        store
            .commit(key, pending, Validators::default())
            .await
            .unwrap()
            .2
    }

    #[tokio::test]
    async fn committed_objects_survive_reopen() {
        let (root, store) = store(1024, 1024).await;
        put(&store, "rustup/dist/a", b"payload").await;
        let (meta, path) = store.get("rustup/dist/a").unwrap();
        assert_eq!(meta.size, 7);
        assert_eq!(fs::read(path).await.unwrap(), b"payload");
        drop(store);
        let (reopened, invalid) = Store::open(&root.path().join("store"), 1024, 1024)
            .await
            .unwrap();
        assert_eq!(invalid, 0);
        assert_eq!(reopened.usage().objects, 1);
    }

    #[tokio::test]
    async fn oversized_and_abandoned_downloads_never_become_objects() {
        let (root, store) = store(1024, 4).await;
        let mut pending = store.begin().await.unwrap();
        assert_eq!(pending.write(b"12345").await, Err(StoreError::TooLarge));
        drop(pending);
        let mut abandoned = store.begin().await.unwrap();
        abandoned.write(b"12").await.unwrap();
        drop(abandoned);
        assert!(store.get("any").is_none());
        let mut tmp = fs::read_dir(root.path().join("store/tmp")).await.unwrap();
        assert!(tmp.next_entry().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn eviction_removes_least_recently_used_objects() {
        let (_root, store) = store(10, 10).await;
        put(&store, "k/old", b"aaaa").await;
        put(&store, "k/used", b"bbbb").await;
        {
            let mut index = store.index.lock().unwrap();
            for meta in index.entries.values_mut() {
                meta.last_access = if meta.key == "k/old" { 1 } else { 2 };
            }
        }
        let eviction = put(&store, "k/new", b"cccc").await;
        assert_eq!(
            eviction,
            Eviction {
                objects: 1,
                bytes: 4
            }
        );
        assert!(store.get("k/old").is_none());
        assert!(store.get("k/used").is_some());
        assert_eq!(store.usage().bytes, 8);
    }

    #[tokio::test]
    async fn refuses_an_unowned_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("foreign"), b"data").unwrap();
        assert!(matches!(
            Store::open(root.path(), 10, 10).await,
            Err(StoreError::Unowned)
        ));
    }

    #[tokio::test]
    async fn discards_metadata_without_its_object() {
        let (root, store) = store(1024, 1024).await;
        put(&store, "k/a", b"abc").await;
        let (_, object) = store.get("k/a").unwrap();
        drop(store);
        std::fs::remove_file(object).unwrap();
        let (reopened, invalid) = Store::open(&root.path().join("store"), 1024, 1024)
            .await
            .unwrap();
        assert_eq!((invalid, reopened.usage().objects), (1, 0));
    }
}
