//! The [`Files`] handle: store lifecycle, imports, exports and local queries.

use std::{
    collections::HashMap,
    fmt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use iroh::{Endpoint, address_lookup::MemoryLookup};
use iroh_blobs::{
    BlobFormat, BlobsProtocol, HashAndFormat,
    api::{
        TempTag,
        blobs::{AddPathOptions, AddProgressItem, ImportMode},
    },
    provider::events::EventSender,
    store::{
        GcConfig,
        fs::{FsStore, options::Options},
    },
    util::connection_pool::{self, ConnectionPool},
};
use n0_future::{Stream, StreamExt};
use parking_lot::Mutex;
use tincan_core::{BlobHash, UserId, wire::AddrHint};
use tokio::{
    sync::{Semaphore, broadcast},
    task::JoinHandle,
};
use tracing::{debug, info, warn};

use crate::{
    AccessPolicy, BlobKind, FileEvent, FilesProtocol, Imported, LocalStatus, TransferInfo, TransferState,
    addr::{blob_hash, blobs_hash},
    download::{self, Download},
    error::{FilesError, Result, store_err},
    provider::{self, ProviderShared},
    throttle::UploadLimiter,
};

/// Downloader connect timeout: a dial to an offline provider costs this long.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
/// Downloads running at the same time; the rest wait in [`TransferState::Queued`].
const MAX_ACTIVE_DOWNLOADS: usize = 3;
/// Garbage collection interval. Only untagged blobs (removed, or never tagged) are collected.
const GC_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Capacity of the [`FileEvent`] broadcast channel.
const EVENT_CHANNEL: usize = 512;
/// Upper bound for letting the store settle before it is shut down.
const SHUTDOWN_IDLE_WAIT: Duration = Duration::from_secs(2);

/// Handle to the local blob store and the download manager. Cheap to clone; all clones
/// share one store.
///
/// Call [`Files::shutdown`] (or shut down the Router it is registered on) before exit:
/// download tasks keep the store alive until then, and only a clean shutdown persists the
/// most recent partial progress.
#[derive(Clone)]
pub struct Files {
    pub(crate) inner: Arc<Inner>,
}

impl fmt::Debug for Files {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Files")
            .field("endpoint", &self.inner.endpoint.id().fmt_short().to_string())
            .field("closed", &self.inner.closed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Kinds a held blob is tagged as.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Kinds {
    attachment: bool,
    clip: bool,
}

impl Kinds {
    pub(crate) fn of(kind: BlobKind) -> Self {
        let mut k = Self::default();
        k.insert(kind);
        k
    }
    pub(crate) fn has(&self, kind: BlobKind) -> bool {
        match kind {
            BlobKind::Attachment => self.attachment,
            BlobKind::Clip => self.clip,
        }
    }
    /// Adds `kind`; true if it was not there yet.
    pub(crate) fn insert(&mut self, kind: BlobKind) -> bool {
        let slot = match kind {
            BlobKind::Attachment => &mut self.attachment,
            BlobKind::Clip => &mut self.clip,
        };
        !std::mem::replace(slot, true)
    }
    pub(crate) fn remove(&mut self, kind: BlobKind) {
        match kind {
            BlobKind::Attachment => self.attachment = false,
            BlobKind::Clip => self.clip = false,
        }
    }
    pub(crate) fn is_empty(&self) -> bool {
        !self.attachment && !self.clip
    }
    pub(crate) fn iter(self) -> impl Iterator<Item = BlobKind> {
        BlobKind::ALL.into_iter().filter(move |k| self.has(*k))
    }
}

/// A complete, tagged blob.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Held {
    pub(crate) kinds: Kinds,
    pub(crate) size: u64,
}

/// Mutable state behind one lock. Never held across an await.
#[derive(Default)]
pub(crate) struct State {
    /// Complete blobs with at least one tag. Source of [`Files::complete_hashes`].
    pub(crate) held: HashMap<BlobHash, Held>,
    /// Download requests that are queued, running, parked or failed.
    pub(crate) downloads: HashMap<BlobHash, Download>,
}

pub(crate) struct Inner {
    pub(crate) store: FsStore,
    pub(crate) endpoint: Endpoint,
    pub(crate) lookup: MemoryLookup,
    pub(crate) pool: ConnectionPool,
    pub(crate) slots: Arc<Semaphore>,
    pub(crate) state: Mutex<State>,
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) next_id: AtomicU64,
    events: broadcast::Sender<FileEvent>,
    blobs: BlobsProtocol,
    provider: Arc<ProviderShared>,
    event_task: Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
    /// Serializes shutdown; true once it completed.
    shutdown_done: tokio::sync::Mutex<bool>,
}

impl Inner {
    pub(crate) fn emit(&self, ev: FileEvent) {
        // An error only means nobody is subscribed right now.
        let _ = self.events.send(ev);
    }
}

impl Files {
    /// Opens (or creates) the blob store in `dir` and prepares serving and downloading on
    /// `endpoint`. Does not spawn a Router: register [`Files::protocol`] on the app's Router
    /// under [`iroh_blobs::ALPN`].
    ///
    /// Adds an in-memory address lookup to `endpoint`, fed from the [`AddrHint`]s given to
    /// [`Files::download`] and [`Files::add_providers`], so providers are dialable without
    /// global discovery.
    ///
    /// # Threads
    ///
    /// The iroh-blobs `FsStore` builds its own tokio runtime sized by the process-wide
    /// `TOKIO_WORKER_THREADS` variable (one thread per core when unset). The app must set
    /// `TOKIO_WORKER_THREADS=2` at the very start of `main`, before any thread or runtime
    /// exists, and build its own runtime with an explicit worker count.
    ///
    /// Must be called inside a tokio runtime; background tasks run on that runtime.
    pub async fn load(dir: &Path, endpoint: Endpoint, policy: Arc<dyn AccessPolicy>) -> Result<Files> {
        let lookup = MemoryLookup::with_provenance("tincan-files");
        endpoint.address_lookup().map_err(|e| FilesError::Endpoint(e.to_string()))?.add(lookup.clone());

        tokio::fs::create_dir_all(dir).await?;
        let mut options = Options::new(dir);
        options.gc = Some(GcConfig { interval: GC_INTERVAL, add_protected: None });
        let store = FsStore::load_with_opts(dir.join("blobs.db"), options).await.map_err(store_err)?;

        let held = match scan_held(&store).await {
            Ok(h) => h,
            Err(e) => {
                if let Err(se) = store.shutdown().await {
                    warn!("closing blob store after failed load: {se}");
                }
                return Err(e);
            }
        };

        let mut pool_options = connection_pool::Options::default();
        pool_options.connect_timeout = CONNECT_TIMEOUT;
        let pool = ConnectionPool::new(endpoint.clone(), iroh_blobs::ALPN, pool_options);

        let provider = Arc::new(ProviderShared {
            policy,
            limiter: UploadLimiter::new(),
            uploaded: AtomicU64::new(0),
        });
        let (sender, rx) = EventSender::channel(provider::EVENT_CAPACITY, provider::EVENT_MASK);
        let blobs = BlobsProtocol::new(&store, Some(sender));
        let rt = tokio::runtime::Handle::current();
        let event_task = rt.spawn(provider::run_events(rx, provider.clone()));
        let (events, _) = broadcast::channel(EVENT_CHANNEL);

        info!(dir = %dir.display(), held = held.len(), "blob store loaded");
        Ok(Files {
            inner: Arc::new(Inner {
                store,
                endpoint,
                lookup,
                pool,
                slots: Arc::new(Semaphore::new(MAX_ACTIVE_DOWNLOADS)),
                state: Mutex::new(State { held, downloads: HashMap::new() }),
                rt,
                next_id: AtomicU64::new(1),
                events,
                blobs,
                provider,
                event_task: Mutex::new(Some(event_task)),
                closed: AtomicBool::new(false),
                shutdown_done: tokio::sync::Mutex::new(false),
            }),
        })
    }

    /// The protocol handler to register on the shared Router under [`iroh_blobs::ALPN`].
    /// Router shutdown also shuts this store down.
    pub fn protocol(&self) -> FilesProtocol {
        FilesProtocol { blobs: self.inner.blobs.clone(), files: self.clone() }
    }

    /// Imports a local file without copying it (`ImportMode::TryReference`; small files are
    /// stored inline). The file is hashed as a stream, never loaded whole into memory, and
    /// must not be modified or moved afterwards, or serving it fails. Tags it as `kind`.
    pub async fn import(&self, path: &Path, kind: BlobKind) -> Result<Imported> {
        self.check_open()?;
        let path = std::path::absolute(path)?;
        let progress = self.inner.store.blobs().add_path_with_opts(AddPathOptions {
            path,
            format: BlobFormat::Raw,
            mode: ImportMode::TryReference,
        });
        let stream = progress.stream().await;
        self.finish_import(stream, kind).await
    }

    /// Imports bytes held in memory (copied into the store). Tags them as `kind`.
    pub async fn import_bytes(&self, bytes: Bytes, kind: BlobKind) -> Result<Imported> {
        self.check_open()?;
        let stream = self.inner.store.blobs().add_bytes(bytes).stream().await;
        self.finish_import(stream, kind).await
    }

    async fn finish_import(&self, stream: impl Stream<Item = AddProgressItem>, kind: BlobKind) -> Result<Imported> {
        tokio::pin!(stream);
        let mut size = None;
        let mut temp: Option<TempTag> = None;
        while let Some(item) = stream.next().await {
            match item {
                AddProgressItem::Size(s) => size = Some(s),
                AddProgressItem::Done(tt) => {
                    temp = Some(tt);
                    break;
                }
                AddProgressItem::Error(e) => return Err(store_err(e)),
                AddProgressItem::CopyProgress(_) | AddProgressItem::CopyDone | AddProgressItem::OutboardProgress(_) => {}
            }
        }
        let Some(tt) = temp else {
            return Err(FilesError::Store("import ended without a result".into()));
        };
        let hash = blob_hash(&tt.hash());
        let size = match size {
            Some(s) => s,
            None => self.status(hash).await?.total.unwrap_or(0),
        };
        // The temp tag protects the content until the named tag exists.
        self.set_tag(hash, kind).await?;
        drop(tt);

        // A download of the same hash is now pointless; take over its kinds.
        let (pending_kinds, had_download) = {
            let mut st = self.inner.state.lock();
            match st.downloads.remove(&hash) {
                Some(d) => {
                    d.abort();
                    (d.kinds, true)
                }
                None => (Kinds::default(), false),
            }
        };
        for k in pending_kinds.iter().filter(|k| *k != kind) {
            self.set_tag(hash, k).await?;
        }
        let mut kinds = pending_kinds;
        kinds.insert(kind);
        let added = self.mark_held(hash, kinds, size);
        if had_download {
            self.inner.emit(FileEvent::Transfer(complete_info(hash, kind, size)));
        }
        for k in added {
            self.inner.emit(FileEvent::Completed { hash, kind: k });
        }
        debug!(hash = %hash.short(), size, ?kind, "imported");
        Ok(Imported { hash, size })
    }

    /// What the local store holds of `hash` right now.
    pub async fn status(&self, hash: BlobHash) -> Result<LocalStatus> {
        self.check_open()?;
        let bf = self.inner.store.blobs().observe(blobs_hash(&hash)).await.map_err(store_err)?;
        let size = bf.size();
        let complete = bf.is_complete();
        Ok(LocalStatus {
            complete,
            have: bf.total_bytes(),
            total: if size > 0 || complete { Some(size) } else { None },
        })
    }

    /// Queues a download of `hash` from `providers`, tried in the given order (put the
    /// author first). Idempotent: a request for a hash already queued or running merges
    /// the providers; one that failed locally is restarted; one already complete with this
    /// kind only re-sends its `Complete` state.
    ///
    /// Each provider's [`AddrHint`] is added to the endpoint's address lookup. Progress and
    /// state changes arrive on [`Files::subscribe`]. When all providers fail the request
    /// turns [`TransferState::Unavailable`] and waits for [`Files::add_providers`].
    pub fn download(&self, hash: BlobHash, kind: BlobKind, providers: Vec<(UserId, AddrHint)>) {
        if self.is_closed() {
            warn!(hash = %hash.short(), "download after shutdown ignored");
            return;
        }
        download::request(&self.inner, hash, kind, providers);
    }

    /// Adds holders for `hash`. Wakes a parked download; providers already tried are tried
    /// again (a re-announced holder is assumed back online). Without a download request
    /// for `hash` this only records the addresses.
    pub fn add_providers(&self, hash: BlobHash, providers: Vec<(UserId, AddrHint)>) {
        if self.is_closed() {
            return;
        }
        download::add_providers(&self.inner, hash, providers);
    }

    /// Stops downloading `hash`. Partial data and its tag are kept, so a later
    /// [`Files::download`] resumes; use [`Files::remove`] to drop them.
    pub fn cancel(&self, hash: BlobHash) {
        let removed = self.inner.state.lock().downloads.remove(&hash);
        if let Some(d) = removed {
            d.abort();
            debug!(hash = %hash.short(), "download cancelled");
        }
    }

    /// Snapshot of the download request for `hash`, or a `Complete` snapshot if the blob is
    /// held. `None` if neither.
    pub fn transfer(&self, hash: BlobHash) -> Option<TransferInfo> {
        let st = self.inner.state.lock();
        if let Some(d) = st.downloads.get(&hash) {
            return Some(d.info(hash));
        }
        st.held.get(&hash).map(|h| {
            let kind = h.kinds.iter().next().unwrap_or(BlobKind::Attachment);
            complete_info(hash, kind, h.size)
        })
    }

    /// Copies a complete blob to `dest` (made absolute; parent directories are created).
    pub async fn export(&self, hash: BlobHash, dest: &Path) -> Result<()> {
        self.ensure_complete(hash).await?;
        let dest = std::path::absolute(dest)?;
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        self.inner.store.blobs().export(blobs_hash(&hash), &dest).finish().await.map_err(store_err)?;
        Ok(())
    }

    /// Reads a complete blob into memory (soundboard clips). Fails with
    /// [`FilesError::TooLarge`] if it is larger than `max` bytes.
    pub async fn read_to_bytes(&self, hash: BlobHash, max: u64) -> Result<Bytes> {
        let size = self.ensure_complete(hash).await?;
        if size > max {
            return Err(FilesError::TooLarge { size, max });
        }
        self.inner.store.blobs().get_bytes(blobs_hash(&hash)).await.map_err(store_err)
    }

    /// Hashes held completely, for `Have` announcements. In memory; updated on import,
    /// download completion and removal.
    pub fn complete_hashes(&self) -> Vec<BlobHash> {
        self.inner.state.lock().held.keys().copied().collect()
    }

    /// True if `hash` is held completely (any kind).
    pub fn is_complete(&self, hash: &BlobHash) -> bool {
        self.inner.state.lock().held.contains_key(hash)
    }

    /// Tagged blobs that are not complete and not being downloaded: downloads interrupted by
    /// a restart or [`Files::cancel`]. The app resumes them with [`Files::download`] once it
    /// knows holders.
    pub async fn partial_downloads(&self) -> Result<Vec<(BlobHash, BlobKind)>> {
        self.check_open()?;
        let mut out = Vec::new();
        for kind in BlobKind::ALL {
            let tags = list_tags(&self.inner.store, kind).await?;
            for hash in tags {
                let skip = {
                    let st = self.inner.state.lock();
                    st.held.contains_key(&hash) || st.downloads.contains_key(&hash)
                };
                if skip {
                    continue;
                }
                if !self.status(hash).await?.complete {
                    out.push((hash, kind));
                }
            }
        }
        Ok(out)
    }

    /// Forgets `hash`: cancels its download and drops both tags (attachment and clip). The
    /// data is deleted by the next garbage collection run (hourly). Referenced files
    /// (imported with `TryReference`) are never deleted, only forgotten.
    pub async fn remove(&self, hash: BlobHash) -> Result<()> {
        self.check_open()?;
        {
            let mut st = self.inner.state.lock();
            if let Some(d) = st.downloads.remove(&hash) {
                d.abort();
            }
            st.held.remove(&hash);
        }
        for kind in BlobKind::ALL {
            self.inner.store.tags().delete(kind.tag_name(&hash)).await.map_err(store_err)?;
        }
        debug!(hash = %hash.short(), "removed");
        Ok(())
    }

    /// Drops only the `kind` tag of `hash`. The blob stays held if the other kind still tags
    /// it. A download that was requested only as `kind` is cancelled.
    pub async fn remove_kind(&self, hash: BlobHash, kind: BlobKind) -> Result<()> {
        self.check_open()?;
        {
            let mut st = self.inner.state.lock();
            let cancel = match st.downloads.get_mut(&hash) {
                Some(d) => {
                    d.kinds.remove(kind);
                    d.kinds.is_empty()
                }
                None => false,
            };
            if cancel && let Some(d) = st.downloads.remove(&hash) {
                d.abort();
            }
            let drop_held = match st.held.get_mut(&hash) {
                Some(h) => {
                    h.kinds.remove(kind);
                    h.kinds.is_empty()
                }
                None => false,
            };
            if drop_held {
                st.held.remove(&hash);
            }
        }
        self.inner.store.tags().delete(kind.tag_name(&hash)).await.map_err(store_err)?;
        Ok(())
    }

    /// Caps the total upload rate to peers (`None` = unlimited). Applies to transfers in
    /// progress from their next chunk. Values below 1 KiB/s are raised to 1 KiB/s.
    pub fn set_upload_limit(&self, bytes_per_sec: Option<u64>) {
        self.inner.provider.limiter.set_limit(bytes_per_sec);
        info!(?bytes_per_sec, "upload limit set");
    }

    /// The current upload limit in bytes per second.
    pub fn upload_limit(&self) -> Option<u64> {
        self.inner.provider.limiter.limit()
    }

    /// Payload bytes served to peers since load.
    pub fn uploaded_bytes(&self) -> u64 {
        self.inner.provider.uploaded.load(Ordering::Relaxed)
    }

    /// Subscribes to [`FileEvent`]s. A receiver that falls more than 512 events behind gets
    /// `RecvError::Lagged` and should re-query [`Files::transfer`].
    pub fn subscribe(&self) -> broadcast::Receiver<FileEvent> {
        self.inner.events.subscribe()
    }

    /// True once [`Files::shutdown`] started.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Stops all downloads (keeping partial progress), stops serving, flushes and closes the
    /// store. Idempotent and safe to call concurrently; Router shutdown calls it too. After
    /// it returns the same directory can be loaded again.
    pub async fn shutdown(&self) {
        let mut done = self.inner.shutdown_done.lock().await;
        if *done {
            return;
        }
        self.inner.closed.store(true, Ordering::Release);
        let tasks: Vec<JoinHandle<()>> = {
            let mut st = self.inner.state.lock();
            st.downloads.drain().filter_map(|(_, mut d)| d.take_task()).collect()
        };
        for t in &tasks {
            t.abort();
        }
        for t in tasks {
            // Aborted tasks resolve with a cancellation error; nothing to report.
            let _ = t.await;
        }
        let event_task = self.inner.event_task.lock().take();
        if let Some(t) = event_task {
            t.abort();
            let _ = t.await;
        }
        let store = &self.inner.store;
        if tokio::time::timeout(SHUTDOWN_IDLE_WAIT, store.wait_idle()).await.is_err() {
            debug!("blob store still busy at shutdown");
        }
        if let Err(e) = store.sync_db().await {
            warn!("blob store sync at shutdown: {e}");
        }
        if let Err(e) = store.shutdown().await {
            warn!("blob store shutdown: {e}");
        }
        *done = true;
        info!("blob store shut down");
    }

    fn check_open(&self) -> Result<()> {
        if self.is_closed() { Err(FilesError::Closed) } else { Ok(()) }
    }

    /// Size of `hash` if complete locally, else [`FilesError::NotComplete`].
    async fn ensure_complete(&self, hash: BlobHash) -> Result<u64> {
        let st = self.status(hash).await?;
        match (st.complete, st.total) {
            (true, Some(size)) => Ok(size),
            _ => Err(FilesError::NotComplete(hash)),
        }
    }

    async fn set_tag(&self, hash: BlobHash, kind: BlobKind) -> Result<()> {
        set_tag(&self.inner.store, hash, kind).await
    }

    /// Records `hash` as held with `kinds`; returns the kinds that are new.
    fn mark_held(&self, hash: BlobHash, kinds: Kinds, size: u64) -> Vec<BlobKind> {
        mark_held(&mut self.inner.state.lock(), hash, kinds, size)
    }
}

/// Sets the named tag of `kind` for `hash` (works before the blob exists, which protects a
/// partial download from garbage collection).
pub(crate) async fn set_tag(store: &FsStore, hash: BlobHash, kind: BlobKind) -> Result<()> {
    store
        .tags()
        .set(kind.tag_name(&hash), HashAndFormat::raw(blobs_hash(&hash)))
        .await
        .map_err(store_err)
}

/// Records `hash` as held with `kinds`; returns the kinds that are new.
pub(crate) fn mark_held(st: &mut State, hash: BlobHash, kinds: Kinds, size: u64) -> Vec<BlobKind> {
    let held = st.held.entry(hash).or_insert(Held { kinds: Kinds::default(), size });
    held.size = size;
    kinds.iter().filter(|k| held.kinds.insert(*k)).collect()
}

/// A `Complete` transfer snapshot.
pub(crate) fn complete_info(hash: BlobHash, kind: BlobKind, size: u64) -> TransferInfo {
    TransferInfo { hash, kind, state: TransferState::Complete, have: size, total: Some(size), peers: 0 }
}

/// Hashes tagged as `kind`.
async fn list_tags(store: &FsStore, kind: BlobKind) -> Result<Vec<BlobHash>> {
    let stream = store.tags().list_prefix(kind.tag_prefix()).await.map_err(store_err)?;
    tokio::pin!(stream);
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let info = item.map_err(store_err)?;
        out.push(blob_hash(&info.hash));
    }
    Ok(out)
}

/// Builds the held set from the tags and the store's completeness at load.
async fn scan_held(store: &FsStore) -> Result<HashMap<BlobHash, Held>> {
    let mut tagged: HashMap<BlobHash, Kinds> = HashMap::new();
    for kind in BlobKind::ALL {
        for hash in list_tags(store, kind).await? {
            tagged.entry(hash).or_default().insert(kind);
        }
    }
    let mut held = HashMap::new();
    for (hash, kinds) in tagged {
        let bf = store.blobs().observe(blobs_hash(&hash)).await.map_err(store_err)?;
        if bf.is_complete() {
            held.insert(hash, Held { kinds, size: bf.size() });
        }
    }
    Ok(held)
}
