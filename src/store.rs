//! Object store on the cache volume. Objects become visible only through an atomic
//! rename after a complete, size-bounded write; the volume holds acceleration state only.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex as AsyncMutex, OwnedMutexGuard, watch},
};

const MARKER: &str = ".nds-cache-store";
const FOLLOW_CHUNK: u64 = 64 * 1024;
pub(crate) const MIN_RETENTION: u64 = 86_400;
const MARKER_CONTENT: &str = "{\"schema\":1,\"owner\":\"nddev-device-sync-cache\"}\n";

const BUILD_MARKER_CONTENT: &str = "{\"schema\":1,\"owner\":\"nddev-device-sync-cache-builds\"}\n";

static PENDING: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StoreError {
    #[error("store_root_unowned")]
    Unowned,
    #[error("store_io")]
    Io,
    #[error("store_object_too_large")]
    TooLarge,
    #[error("store_capacity")]
    Full,
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

fn idle(meta: &Meta, time: u64, retention: u64) -> bool {
    time.checked_sub(meta.last_access.max(meta.fetched_at))
        .is_some_and(|age| age >= retention)
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

#[derive(Clone, Copy, Debug, Default)]
pub struct Collection {
    pub removed_objects: usize,
    pub removed_bytes: u64,
    pub failed_objects: usize,
    pub busy_objects: usize,
}

struct Index {
    entries: HashMap<String, Meta>,
    readers: HashMap<String, usize>,
    bytes: u64,
}

type Stored = (Meta, PathBuf);

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
    capacity: u64,
    max_object: u64,
    index: Arc<Mutex<Index>>,
    mutation: Arc<AsyncMutex<()>>,
    retention: u64,
    observation_started: Option<u64>,
}

/// A live immutable-object response. Capacity and idle eviction keep this key until drop.
pub struct ReadLease {
    index: Arc<Mutex<Index>>,
    hash: String,
}
impl Drop for ReadLease {
    fn drop(&mut self) {
        if let Ok(mut index) = self.index.lock()
            && let Some(count) = index.readers.get_mut(&self.hash)
        {
            *count -= 1;
            if *count == 0 {
                index.readers.remove(&self.hash);
            }
            if let Some(meta) = index.entries.get_mut(&self.hash) {
                meta.last_access = meta.last_access.max(now());
            }
        }
    }
}

/// An in-progress download. Dropping it without `commit` removes the partial file.
pub struct Pending {
    path: PathBuf,
    file: Option<fs::File>,
    hasher: Sha256,
    size: u64,
    limit: u64,
}

/// Published state of a download that readers follow before it is committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// Bytes flushed to the temporary file so far.
    Writing(u64),
    /// The complete object and where it now lives.
    Committed {
        size: u64,
        path: PathBuf,
    },
    Failed,
}

impl Pending {
    /// The SHA-256 of the bytes written so far, in lowercase hex.
    pub fn sha256(&self) -> String {
        self.hasher
            .clone()
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Makes written bytes visible to readers of the temporary file.
    pub async fn flush(&mut self) -> Result<(), StoreError> {
        self.file.as_mut().ok_or(StoreError::Io)?.flush().await?;
        Ok(())
    }

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

/// Streams a download while it is written. The temporary file is opened while it exists;
/// once the rename happened, the committed object is opened instead. The stream ends with an
/// error when the download fails or its writer disappears, so a client never takes a partial
/// object for a complete one.
pub async fn follow(
    temp: &Path,
    mut progress: watch::Receiver<Progress>,
) -> std::io::Result<impl Stream<Item = std::io::Result<Bytes>> + Send + 'static> {
    let failed = || std::io::Error::other("download_failed");
    let file = match fs::File::open(temp).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let state = progress
                .wait_for(|state| !matches!(state, Progress::Writing(_)))
                .await
                .map_err(|_| failed())?
                .clone();
            match state {
                Progress::Committed { path, .. } => fs::File::open(path).await?,
                _ => return Err(failed()),
            }
        }
        Err(error) => return Err(error),
    };
    Ok(futures_util::stream::unfold(
        Some((file, progress, 0u64)),
        move |state| async move {
            let (mut file, mut progress, offset) = state?;
            loop {
                let (limit, end) = match &*progress.borrow_and_update() {
                    Progress::Writing(written) => (*written, None),
                    Progress::Committed { size, .. } => (*size, Some(true)),
                    Progress::Failed => (offset, Some(false)),
                };
                if offset < limit {
                    let mut buffer = vec![0; (limit - offset).min(FOLLOW_CHUNK) as usize];
                    return match file.read(&mut buffer).await {
                        Ok(0) => Some((Err(failed()), None)),
                        Ok(read) => {
                            buffer.truncate(read);
                            let next = offset + read as u64;
                            Some((Ok(Bytes::from(buffer)), Some((file, progress, next))))
                        }
                        Err(error) => Some((Err(error), None)),
                    };
                }
                match end {
                    Some(true) => return None,
                    Some(false) => return Some((Err(failed()), None)),
                    None => {
                        if progress.changed().await.is_err() {
                            return Some((Err(failed()), None));
                        }
                    }
                }
            }
        },
    ))
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
        Self::open_policy(root, capacity, max_object, true, MARKER_CONTENT).await
    }

    /// A separately owned build store keeps unknown metadata for its owner to inspect.
    pub async fn open_preserving_unknown(
        root: &Path,
        capacity: u64,
        max_object: u64,
    ) -> Result<(Self, usize), StoreError> {
        Self::open_policy(root, capacity, max_object, false, BUILD_MARKER_CONTENT).await
    }

    async fn open_policy(
        root: &Path,
        capacity: u64,
        max_object: u64,
        discard_invalid: bool,
        marker_content: &str,
    ) -> Result<(Self, usize), StoreError> {
        fs::create_dir_all(root).await?;
        let marker = root.join(MARKER);
        match fs::read_to_string(&marker).await {
            Ok(content) if content == marker_content => {}
            Ok(_) => return Err(StoreError::Unowned),
            Err(_) => {
                let mut entries = fs::read_dir(root).await?;
                if entries.next_entry().await?.is_some() {
                    return Err(StoreError::Unowned);
                }
                fs::write(&marker, marker_content).await?;
            }
        }
        for directory in ["objects", "meta"] {
            fs::create_dir_all(root.join(directory)).await?;
        }
        let _ = fs::remove_dir_all(root.join("tmp")).await;
        fs::create_dir_all(root.join("tmp")).await?;

        let (entries, invalid) = Self::load(root, discard_invalid).await?;
        let bytes = entries.values().map(|meta| meta.size).sum();
        Ok((
            Self {
                root: root.to_owned(),
                capacity,
                max_object,
                mutation: Arc::new(AsyncMutex::new(())),
                retention: MIN_RETENTION,
                // Download hits were volatile before this process opened.
                observation_started: discard_invalid.then(now),
                index: Arc::new(Mutex::new(Index {
                    entries,
                    bytes,
                    readers: HashMap::new(),
                })),
            },
            invalid,
        ))
    }

    async fn load(
        root: &Path,
        discard_invalid: bool,
    ) -> Result<(HashMap<String, Meta>, usize), StoreError> {
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
                    if discard_invalid {
                        let _ = fs::remove_file(&path).await;
                    }
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
                        if discard_invalid {
                            let _ = fs::remove_file(&path).await;
                            let _ = fs::remove_file(&object).await;
                        }
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
        meta.last_access = meta.last_access.max(now());
        Some((meta.clone(), Self::object_path(&self.root, &hash)))
    }

    pub fn lease(&self, key: &str) -> Option<(Meta, PathBuf, ReadLease)> {
        let (stored, lease) = self.protect(key).ok()?;
        stored.map(|(meta, path)| (meta, path, lease))
    }

    /// Pin a key before looking it up, including a response that follows an
    /// uncommitted fill. Taking the lease and taking the snapshot are atomic.
    pub(crate) fn protect(&self, key: &str) -> Result<(Option<Stored>, ReadLease), StoreError> {
        let hash = digest(key.as_bytes());
        let mut index = self.index.lock().map_err(|_| StoreError::Io)?;
        let stored = index.entries.get_mut(&hash).map(|meta| {
            meta.last_access = meta.last_access.max(now());
            (meta.clone(), Self::object_path(&self.root, &hash))
        });
        *index.readers.entry(hash.clone()).or_default() += 1;
        Ok((
            stored,
            ReadLease {
                index: self.index.clone(),
                hash,
            },
        ))
    }

    pub(crate) fn with_retention(mut self, seconds: u64) -> Self {
        self.retention = seconds.max(MIN_RETENTION);
        self
    }

    fn needed(&self, index: &Index, incoming: &str, size: u64) -> u64 {
        let replaced = index.entries.get(incoming).map_or(0, |meta| meta.size);
        (index.bytes - replaced)
            .saturating_add(size)
            .saturating_sub(self.capacity)
    }

    fn quota_eviction_allowed(&self, time: u64) -> bool {
        self.observation_started.is_none_or(|start| {
            time.checked_sub(start)
                .is_some_and(|age| age >= self.retention)
        })
    }

    fn victim(&self, index: &Index, hash: &str, meta: &Meta, incoming: &str, time: u64) -> bool {
        hash != incoming && !index.readers.contains_key(hash) && idle(meta, time, self.retention)
    }

    /// Avoid streaming a known-length body if admission is already impossible.
    /// The final commit rechecks this under the mutation guard.
    pub(crate) fn preflight(&self, key: &str, size: u64, replace: bool) -> Result<(), StoreError> {
        let incoming = digest(key.as_bytes());
        let index = self.index.lock().map_err(|_| StoreError::Io)?;
        if !replace && index.entries.contains_key(&incoming) {
            // Build PUT still verifies the digest before answering an existing key.
            return Ok(());
        }
        let mut needed = self.needed(&index, &incoming, size);
        if needed == 0 {
            return Ok(());
        }
        let time = now();
        if self.quota_eviction_allowed(time) {
            for (hash, meta) in &index.entries {
                if self.victim(&index, hash, meta, &incoming, time) {
                    needed = needed.saturating_sub(meta.size);
                    if needed == 0 {
                        return Ok(());
                    }
                }
            }
        }
        Err(StoreError::Full)
    }

    async fn admit(
        &self,
        incoming: &str,
        size: u64,
        guard: OwnedMutexGuard<()>,
    ) -> Result<(Eviction, OwnedMutexGuard<()>), StoreError> {
        {
            let index = self.index.lock().map_err(|_| StoreError::Io)?;
            if self.needed(&index, incoming, size) == 0 {
                return Ok((Eviction::default(), guard));
            }
        }
        let store = self.clone();
        let incoming = incoming.to_owned();
        // Once victims are reserved, finish their accounting even if the
        // requester disconnects. The owned mutation guard moves with the work.
        tokio::spawn(async move {
            let eviction = store.evict_idle_for_quota(&incoming, size).await?;
            Ok((eviction, guard))
        })
        .await
        .map_err(|_| StoreError::Io)?
    }

    async fn evict_idle_for_quota(
        &self,
        incoming: &str,
        size: u64,
    ) -> Result<Eviction, StoreError> {
        let time = now();
        if !self.quota_eviction_allowed(time) {
            return Err(StoreError::Full);
        }
        let mut candidates = {
            let index = self.index.lock().map_err(|_| StoreError::Io)?;
            index
                .entries
                .iter()
                .filter(|(hash, meta)| self.victim(&index, hash, meta, incoming, time))
                .map(|(hash, meta)| (meta.last_access.max(meta.fetched_at), hash.clone()))
                .collect::<Vec<_>>()
        };
        // Keep sorting off the lookup lock. Hits and new body leases may
        // invalidate candidates while the snapshot is being ranked.
        candidates.sort_unstable();
        let victims = {
            let mut index = self.index.lock().map_err(|_| StoreError::Io)?;
            let mut needed = self.needed(&index, incoming, size);
            let mut selected = Vec::new();
            for (_, hash) in candidates {
                let Some(meta) = index
                    .entries
                    .get(&hash)
                    .filter(|meta| self.victim(&index, &hash, meta, incoming, time))
                else {
                    continue;
                };
                needed = needed.saturating_sub(meta.size);
                selected.push(hash);
                if needed == 0 {
                    break;
                }
            }
            if needed != 0 {
                return Err(StoreError::Full);
            }
            selected
                .into_iter()
                .filter_map(|hash| {
                    let meta = index.entries.remove(&hash)?;
                    index.bytes -= meta.size;
                    Some((hash, meta))
                })
                .collect::<Vec<_>>()
        };
        let mut eviction = Eviction::default();
        let mut victims = victims.into_iter();
        while let Some((hash, meta)) = victims.next() {
            match self.remove_files(&hash).await {
                Ok(removed) => {
                    eviction.objects += 1;
                    if removed {
                        eviction.bytes += meta.size;
                    }
                }
                Err(error) => {
                    // A failed metadata unlink after the payload is gone
                    // must not put the missing object back into the index.
                    let body_retained = fs::symlink_metadata(Self::object_path(&self.root, &hash))
                        .await
                        .is_ok();
                    let mut index = self.index.lock().map_err(|_| StoreError::Io)?;
                    for (hash, meta) in body_retained
                        .then_some((hash, meta))
                        .into_iter()
                        .chain(victims)
                    {
                        index.bytes += meta.size;
                        index.entries.insert(hash, meta);
                    }
                    return Err(error);
                }
            }
        }
        Ok(eviction)
    }

    async fn remove_files(&self, hash: &str) -> Result<bool, StoreError> {
        let removed = match fs::remove_file(Self::object_path(&self.root, hash)).await {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return Err(StoreError::Io),
        };
        match fs::remove_file(Self::meta_path(&self.root, hash)).await {
            Ok(()) => Ok(removed),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(removed),
            Err(_) => Err(StoreError::Io),
        }
    }

    /// Durable last use for the optional build store. Its caller serializes metadata writers.
    pub async fn touch(&self, key: &str) -> Result<(), StoreError> {
        let hash = digest(key.as_bytes());
        let meta = {
            let mut index = self.index.lock().map_err(|_| StoreError::Io)?;
            let meta = index.entries.get_mut(&hash).ok_or(StoreError::Io)?;
            meta.last_access = meta.last_access.max(now());
            meta.clone()
        };
        self.write_meta(&Self::meta_path(&self.root, &hash), &meta)
            .await
    }

    /// Removes only recognized idle objects. The build-store owner excludes active readers and commits.
    pub async fn collect_idle(&self, time: u64, retention: u64) -> Result<Collection, StoreError> {
        let _mutation = self.mutation.lock().await;
        let retention = retention.max(self.retention);
        let (candidates, busy) = {
            let index = self.index.lock().map_err(|_| StoreError::Io)?;
            let mut candidates = Vec::new();
            let mut busy = 0;
            for (hash, meta) in &index.entries {
                if idle(meta, time, retention) {
                    if index.readers.contains_key(hash) {
                        busy += 1;
                    } else {
                        candidates.push((hash.clone(), meta.size));
                    }
                }
            }
            (candidates, busy)
        };
        let mut report = Collection {
            busy_objects: busy,
            ..Default::default()
        };
        for (hash, size) in candidates {
            let removed = match self.remove_files(&hash).await {
                Ok(removed) => removed,
                Err(_) => {
                    report.failed_objects += 1;
                    continue;
                }
            };
            let mut index = self.index.lock().map_err(|_| StoreError::Io)?;
            if index.entries.remove(&hash).is_some() {
                index.bytes -= size;
                report.removed_objects += 1;
                if removed {
                    report.removed_bytes += size;
                }
            }
        }
        Ok(report)
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
        let file = pending.file.as_mut().ok_or(StoreError::Io)?;
        file.flush().await?;
        file.sync_data().await?;
        let hash = digest(key.as_bytes());
        let guard = self.mutation.clone().lock_owned().await;
        let (eviction, _guard) = self.admit(&hash, pending.size, guard).await?;
        let object = Self::object_path(&self.root, &hash);
        let meta_path = Self::meta_path(&self.root, &hash);
        let time = now();
        let meta = Meta {
            key: key.to_owned(),
            size: pending.size,
            sha256: pending.sha256(),
            content_type: validators.content_type,
            etag: validators.etag,
            last_modified: validators.last_modified,
            fetched_at: time,
            last_access: time,
        };
        fs::create_dir_all(object.parent().ok_or(StoreError::Io)?).await?;
        fs::create_dir_all(meta_path.parent().ok_or(StoreError::Io)?).await?;
        fs::rename(&pending.path, &object).await?;
        drop(pending.file.take());
        self.write_meta(&meta_path, &meta).await?;
        {
            let mut index = self.index.lock().map_err(|_| StoreError::Io)?;
            let previous = index
                .entries
                .insert(hash.clone(), meta.clone())
                .map(|old| old.size)
                .unwrap_or(0);
            index.bytes = index.bytes - previous + meta.size;
        }
        Ok((meta, object, eviction))
    }

    /// Records a successful revalidation of a stored object.
    pub async fn refresh(&self, key: &str, validators: Validators) -> Result<Meta, StoreError> {
        let _mutation = self.mutation.lock().await;
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

    pub fn max_object(&self) -> u64 {
        self.max_object
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
        let sequence = PENDING.fetch_add(1, Ordering::Relaxed);
        let probe = self
            .root
            .join("tmp")
            .join(format!(".ready-{}-{sequence}", std::process::id()));
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
        let (store, invalid) =
            Store::open_preserving_unknown(&root.path().join("store"), capacity, max_object)
                .await
                .unwrap();
        assert_eq!(invalid, 0);
        (root, store)
    }

    async fn try_put(store: &Store, key: &str, body: &[u8]) -> Result<Eviction, StoreError> {
        let mut pending = store.begin().await.unwrap();
        pending.write(body).await.unwrap();
        store
            .commit(key, pending, Validators::default())
            .await
            .map(|value| value.2)
    }

    async fn put(store: &Store, key: &str, body: &[u8]) -> Eviction {
        try_put(store, key, body).await.unwrap()
    }

    fn use_at(store: &Store, key: &str, time: u64) {
        let mut index = store.index.lock().unwrap();
        let meta = index.entries.get_mut(&digest(key.as_bytes())).unwrap();
        meta.fetched_at = time;
        meta.last_access = time;
    }

    #[tokio::test]
    async fn pinned_objects_survive_capacity_pressure_and_retry_of_partial_collection() {
        let (root, store) = store(8, 8).await;
        put(&store, "a", b"aaaa").await;
        let (_, path, lease) = store.lease("a").unwrap();
        put(&store, "b", b"bbbb").await;
        let old = now() - MIN_RETENTION - 60;
        use_at(&store, "a", old);
        use_at(&store, "b", old);
        assert_eq!(
            try_put(&store, "c", b"cccccccc").await,
            Err(StoreError::Full)
        );
        assert!(path.exists());
        assert!(Store::object_path(&store.root, &digest(b"b")).exists());
        assert_eq!(store.usage().bytes, 8);
        drop(lease);
        // A completed response renews the protection window.
        assert_eq!(
            try_put(&store, "c", b"cccccccc").await,
            Err(StoreError::Full)
        );
        use_at(&store, "a", old);
        put(&store, "c", b"cccccccc").await;
        assert_eq!(store.usage().bytes, 8);
        let hash = digest(b"c");
        let meta = Store::meta_path(&root.path().join("store"), &hash);
        let original = fs::read(&meta).await.unwrap();
        fs::remove_file(&meta).await.unwrap();
        fs::create_dir(&meta).await.unwrap();
        fs::write(meta.join("keep"), b"foreign").await.unwrap();
        let partial = store.collect_idle(now() + 86401, 86400).await.unwrap();
        assert_eq!(partial.failed_objects, 1);
        assert_eq!(fs::read(meta.join("keep")).await.unwrap(), b"foreign");
        fs::remove_file(meta.join("keep")).await.unwrap();
        fs::remove_dir(&meta).await.unwrap();
        fs::write(&meta, original).await.unwrap();
        let recovered = store.collect_idle(now() + 86401, 86400).await.unwrap();
        assert_eq!(recovered.failed_objects, 0);
        assert_eq!(recovered.removed_objects, 1);
        assert_eq!(recovered.removed_bytes, 0);
        assert_eq!(store.usage().bytes, 0);
    }

    #[tokio::test]
    async fn committed_objects_survive_reopen() {
        let (root, store) = store(1024, 1024).await;
        put(&store, "rustup/dist/a", b"payload").await;
        let (meta, path) = store.get("rustup/dist/a").unwrap();
        assert_eq!(meta.size, 7);
        assert_eq!(fs::read(path).await.unwrap(), b"payload");
        drop(store);
        let (reopened, invalid) =
            Store::open_preserving_unknown(&root.path().join("store"), 1024, 1024)
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
    async fn eviction_removes_oldest_eligible_objects() {
        let (_root, store) = store(10, 10).await;
        put(&store, "k/old", b"aaaa").await;
        put(&store, "k/used", b"bbbb").await;
        use_at(&store, "k/old", now() - MIN_RETENTION - 60);
        use_at(&store, "k/used", now() - MIN_RETENTION - 30);
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
    async fn quota_refusal_preserves_recent_future_and_existing_replacement() {
        let (_root, store) = store(12, 12).await;
        put(&store, "current", b"keep").await;
        put(&store, "old", b"old!").await;
        put(&store, "future", b"safe").await;
        use_at(&store, "old", now() - MIN_RETENTION - 60);
        use_at(&store, "future", now() + 60);
        assert_eq!(store.preflight("other", 5, true), Err(StoreError::Full));
        assert_eq!(
            try_put(&store, "other", b"12345").await,
            Err(StoreError::Full)
        );
        assert_eq!(
            try_put(&store, "current", b"replace!!").await,
            Err(StoreError::Full)
        );
        let (_, path) = store.get("current").unwrap();
        assert_eq!(fs::read(path).await.unwrap(), b"keep");
        assert_eq!(store.usage().bytes, 12);
        assert!(
            fs::read_dir(store.root.join("tmp"))
                .await
                .unwrap()
                .next_entry()
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(put(&store, "current", b"same").await, Eviction::default());
        let eviction = put(&store, "other", b"new!").await;
        assert_eq!(
            eviction,
            Eviction {
                objects: 1,
                bytes: 4
            }
        );
        assert!(store.get("old").is_none());
        assert!(store.get("future").is_some());
    }

    #[tokio::test]
    async fn download_quota_waits_for_restart_observation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("store");
        let (store, _) = Store::open(&root, 4, 4).await.unwrap();
        put(&store, "old", b"old!").await;
        let hash = digest(b"old");
        use_at(&store, "old", now() - MIN_RETENTION - 60);
        let meta = store.index.lock().unwrap().entries[&hash].clone();
        store
            .write_meta(&Store::meta_path(&root, &hash), &meta)
            .await
            .unwrap();
        drop(store);
        let (mut store, _) = Store::open(&root, 4, 4).await.unwrap();
        assert_eq!(try_put(&store, "new", b"new!").await, Err(StoreError::Full));
        store.observation_started = Some(now() - MIN_RETENTION - 30);
        assert_eq!(put(&store, "new", b"new!").await.bytes, 4);
    }

    #[tokio::test]
    async fn concurrent_commits_share_one_capacity_boundary() {
        let (_root, store) = store(4, 4).await;
        let (first, second) =
            tokio::join!(try_put(&store, "a", b"aaaa"), try_put(&store, "b", b"bbbb"));
        assert!(matches!(
            (&first, &second),
            (Ok(_), Err(StoreError::Full)) | (Err(StoreError::Full), Ok(_))
        ));
        assert_eq!(store.usage().bytes, 4);
        assert_eq!(store.usage().objects, 1);
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
        let root = tempfile::tempdir().unwrap();
        let (store, _) = Store::open(&root.path().join("store"), 1024, 1024)
            .await
            .unwrap();
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
