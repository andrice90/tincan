//! The running profile: every subsystem plus the app's own caches.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tincan_core::event::Body;
use tincan_core::storage::Storage;
use tincan_core::{BlobHash, ConvId, UserId};
use tincan_files::{AccessPolicy, BlobKind, Files};
use tincan_net::{Net, NetConfig, NetError, RelayChoice};
use tincan_store::SqliteStorage;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::dto::{self, ToastKind};
use crate::emit::Emitter;
use crate::media::Media;
use crate::profile::{self, BLOBS_DIR, DB_FILE, ProfileLock};
use crate::pump::{self, PumpCmd};
use crate::settings;
use crate::transfers::{self, FetchReq};
use crate::views::{ConvView, Refresh, SentKey};
use crate::{AppConfig, emit::Ev};

/// The attachment index is cleared when it grows past this (it refills from messages).
pub(crate) const MAX_INDEXED_ATTACHMENTS: usize = 50_000;
/// Queue of background fetch and holder lookups.
pub(crate) const FETCH_QUEUE: usize = 1024;
/// Bound on `App::shutdown`'s network part (the handoff takes up to about 3 s).
const NET_SHUTDOWN_BUDGET: Duration = Duration::from_secs(10);

/// Where an attachment can be fetched from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttRef {
    /// The conversation it was posted in (who_has goes there).
    pub conv: ConvId,
    /// Who posted it.
    pub author: UserId,
    /// File name.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// Declared MIME type.
    pub mime: String,
}

/// Shared state of one running profile.
pub(crate) struct Core {
    /// How the profile was opened.
    pub cfg: AppConfig,
    /// My user id.
    pub me: UserId,
    /// Persistence (inherent methods: settings, read markers, contacts, downloads).
    pub storage: Arc<SqliteStorage>,
    /// Networking.
    pub net: Net,
    /// Blob store.
    pub files: Files,
    /// UI events.
    pub events: Emitter,
    /// Current settings.
    pub settings: RwLock<dto::Settings>,
    /// Display name (None until picked).
    pub name: RwLock<Option<String>>,
    /// Cached conversation views.
    pub views: Mutex<HashMap<ConvId, Arc<ConvView>>>,
    /// Who may fetch blobs from me (members of shared servers and DM contacts).
    pub allowed: Arc<RwLock<HashSet<UserId>>>,
    /// Attachment hash index (from messages seen).
    pub atts: Mutex<HashMap<BlobHash, AttRef>>,
    /// Holders from the last `who_has` per hash.
    pub holders: Mutex<HashMap<BlobHash, Vec<UserId>>>,
    /// Last state seen per download (persisted on change).
    pub transfer_states: Mutex<HashMap<BlobHash, tincan_files::TransferState>>,
    /// Last DTO payload sent per key.
    pub sent: Mutex<HashMap<SentKey, serde_json::Value>>,
    /// DM delivery acks already turned into `message-updated`.
    pub dm_acked: Mutex<HashMap<ConvId, u64>>,
    /// Servers with a Profile publish in flight.
    pub profile_inflight: Mutex<HashSet<ConvId>>,
    /// Partial downloads to resume once a server is connected.
    pub resume: Mutex<Option<Vec<(BlobHash, BlobKind)>>>,
    /// Notices from startup, shown after the UI asked for its first snapshot.
    pub notices: Mutex<Vec<String>>,
    /// Voice, calls, screen share, soundboard and devices.
    pub media: Media,
    /// Commands to the event pump.
    pub pump_tx: mpsc::Sender<PumpCmd>,
    /// Background fetch queue.
    pub fetch_tx: mpsc::Sender<FetchReq>,
    /// Long-running tasks (aborted on shutdown).
    pub tasks: Mutex<Vec<JoinHandle<()>>>,
    /// The profile lock (released on shutdown).
    lock: Mutex<Option<ProfileLock>>,
    /// Set once shutdown started.
    closed: AtomicBool,
    /// Shutdown runs once; later callers wait for it.
    shutdown_once: tokio::sync::OnceCell<()>,
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "Core({:?})", self.me) }
}

/// The network configuration for this start.
fn net_config(cfg: &AppConfig, s: &dto::Settings, name: Option<&str>) -> NetConfig {
    let offline = cfg.test.offline_network;
    let relay = if offline {
        RelayChoice::Disabled
    } else {
        match &s.custom_relay_url {
            Some(url) => RelayChoice::Custom(vec![url.clone()]),
            None => RelayChoice::N0Default,
        }
    };
    NetConfig {
        relay,
        mdns: !offline,
        app_version: cfg.app_version.clone(),
        allow_hosting: s.allow_hosting,
        bind_addr: offline.then(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
        display_name: name.unwrap_or("Anonymous").to_string(),
        rendezvous: !offline,
    }
}

impl Core {
    /// Open the profile and start everything (see `App::start`).
    pub(crate) async fn start(cfg: AppConfig) -> Result<Arc<Core>, String> {
        let lock = profile::lock(&cfg.data_dir)?;
        let identity = profile::identity(&cfg.data_dir)?;
        let storage = Arc::new(SqliteStorage::open(cfg.data_dir.join(DB_FILE)).map_err(|e| {
            tracing::error!(error = %e, "opening the database failed");
            "Couldn't open the Tincan database. It may be damaged or in use.".to_string()
        })?);
        let settings = settings::load(&storage);
        let name = profile::load_name(storage.as_ref());
        let mut notices = Vec::new();

        let mut net_cfg = net_config(&cfg, &settings, name.as_deref());
        let endpoint = match tincan_net::build_endpoint(&identity, &net_cfg).await {
            Ok(ep) => ep,
            Err(e) if matches!(net_cfg.relay, RelayChoice::Custom(_)) => {
                tracing::warn!(error = %e, "custom relay rejected; using the default relays");
                notices.push("Your custom relay didn't work, so Tincan is using the default relays. Check the relay URL in Settings.".to_string());
                net_cfg.relay = RelayChoice::N0Default;
                tincan_net::build_endpoint(&identity, &net_cfg).await.map_err(start_err)?
            }
            Err(e) => return Err(start_err(e)),
        };

        let allowed: Arc<RwLock<HashSet<UserId>>> = Arc::new(RwLock::new(HashSet::new()));
        let check = allowed.clone();
        let policy: Arc<dyn AccessPolicy> = Arc::new(move |u: &UserId| check.read().contains(u));
        let files = match Files::load(&cfg.data_dir.join(BLOBS_DIR), endpoint.clone(), policy).await {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(error = %e, "loading the blob store failed");
                endpoint.close().await;
                return Err("Couldn't open the Tincan file store. It may be damaged or in use.".to_string());
            }
        };
        let proto = files.protocol();
        let dyn_storage: Arc<dyn Storage> = storage.clone();
        let net = match Net::start(endpoint, dyn_storage, identity.clone(), net_cfg, |b| b.accept(tincan_files::ALPN, proto)).await {
            Ok(n) => n,
            Err(e) => {
                files.shutdown().await;
                return Err(start_err(e));
            }
        };
        let net_rx = net.subscribe();
        let file_rx = files.subscribe();
        net.set_have(files.complete_hashes());

        let (pump_tx, pump_rx) = mpsc::channel(64);
        let (fetch_tx, fetch_rx) = mpsc::channel(FETCH_QUEUE);
        let me = identity.user();
        let core = Arc::new(Core {
            media: Media::new(&cfg),
            cfg,
            me,
            storage,
            net,
            files,
            events: Emitter::new(),
            settings: RwLock::new(settings),
            name: RwLock::new(name),
            views: Mutex::new(HashMap::new()),
            allowed,
            atts: Mutex::new(HashMap::new()),
            holders: Mutex::new(HashMap::new()),
            transfer_states: Mutex::new(HashMap::new()),
            sent: Mutex::new(HashMap::new()),
            dm_acked: Mutex::new(HashMap::new()),
            profile_inflight: Mutex::new(HashSet::new()),
            resume: Mutex::new(None),
            notices: Mutex::new(notices),
            pump_tx,
            fetch_tx,
            tasks: Mutex::new(Vec::new()),
            lock: Mutex::new(Some(lock)),
            closed: AtomicBool::new(false),
            shutdown_once: tokio::sync::OnceCell::new(),
        });
        core.warm_up();
        core.media.start(&core);
        let pump = tokio::spawn(pump::run(core.clone(), net_rx, file_rx, pump_rx));
        let worker = tokio::spawn(transfers::fetch_worker(core.clone(), fetch_rx));
        core.tasks.lock().extend([pump, worker]);
        match core.files.partial_downloads().await {
            Ok(list) if !list.is_empty() => {
                tracing::info!(count = list.len(), "partial downloads to resume");
                *core.resume.lock() = Some(list);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "listing partial downloads failed"),
        }
        tracing::info!(me = %me.short(), dir = %core.cfg.data_dir.display(), "app started");
        Ok(core)
    }

    /// Build every view, the access set and the DM delivery marks.
    fn warm_up(&self) {
        for conv in self.net.convs() {
            if let Some(v) = self.view(&conv)
                && v.is_dm
            {
                self.dm_acked.lock().insert(conv, self.net.dm_delivered(&conv));
            }
        }
        self.update_policy();
    }

    /// True once shutdown started.
    pub(crate) fn is_closed(&self) -> bool { self.closed.load(Ordering::Acquire) }

    /// Clean shutdown (see `App::shutdown`). Idempotent; concurrent callers wait.
    pub(crate) async fn shutdown(self: &Arc<Self>) {
        self.shutdown_once
            .get_or_init(|| async {
                self.closed.store(true, Ordering::Release);
                tracing::info!("app shutting down");
                if tokio::time::timeout(Duration::from_secs(3), self.media.shutdown(self)).await.is_err() {
                    tracing::warn!("media shutdown timed out");
                }
                if tokio::time::timeout(NET_SHUTDOWN_BUDGET, self.net.shutdown()).await.is_err() {
                    tracing::warn!("network shutdown timed out");
                }
                // Router shutdown already closed the store; this is idempotent and covers a
                // timed-out network shutdown.
                if tokio::time::timeout(Duration::from_secs(5), self.files.shutdown()).await.is_err() {
                    tracing::warn!("file store shutdown timed out");
                }
                for t in self.tasks.lock().drain(..) {
                    t.abort();
                }
                self.lock.lock().take();
                tracing::info!("app stopped");
            })
            .await;
    }

    /// Recompute who may fetch blobs from me.
    pub(crate) fn update_policy(&self) {
        let mut set = HashSet::new();
        for conv in self.net.convs() {
            if let Some(v) = self.view(&conv) {
                if v.is_dm {
                    set.extend(v.peer);
                } else {
                    set.extend(v.members.iter().copied());
                }
            }
        }
        set.remove(&self.me);
        *self.allowed.write() = set;
    }

    /// Ask the pump to treat `conv` as bulk (history sync) until it goes quiet.
    pub(crate) fn arm_bulk(&self, conv: ConvId) { let _ = self.pump_tx.try_send(PumpCmd::ArmBulk(conv)); }

    /// The display name, or the error for commands that need one.
    pub(crate) fn require_name(&self) -> Result<String, String> {
        self.name.read().clone().ok_or_else(|| "Pick a display name first.".to_string())
    }

    /// `set_name`: persist, tell the network and every server.
    pub(crate) fn set_name(self: &Arc<Self>, raw: &str) -> Result<dto::Me, String> {
        let name = raw.trim();
        validate_name(name, "Enter a name")?;
        profile::save_name(self.storage.as_ref(), name)?;
        *self.name.write() = Some(name.to_string());
        self.net.set_display_name(name);
        for conv in self.net.convs() {
            self.ensure_profile(conv);
        }
        self.emit_snapshot();
        self.me_dto().ok_or_else(|| "Couldn't save your name. Try again.".to_string())
    }

    /// Publish my Profile in `server` if the name there differs from mine.
    pub(crate) fn ensure_profile(self: &Arc<Self>, server: ConvId) {
        let Some(name) = self.name.read().clone() else { return };
        let Some(view) = self.view(&server) else { return };
        if view.is_dm || !view.members.contains(&self.me) || view.known_name(&self.me) == Some(name.as_str()) {
            return;
        }
        if !self.profile_inflight.lock().insert(server) {
            return;
        }
        let core = self.clone();
        tokio::spawn(async move {
            if let Err(e) = core.net.publish(server, Body::Profile { name }).await {
                tracing::info!(server = %server.short(), error = %e, "publishing my name failed; will retry on reconnect");
            }
            core.profile_inflight.lock().remove(&server);
        });
    }

    /// A server became connected: resume partial downloads once, fix my name there.
    pub(crate) fn on_connected(self: &Arc<Self>, server: ConvId) {
        self.ensure_profile(server);
        self.resume_downloads();
    }

    /// Queue the partial downloads found at start (once, on the first connection).
    pub(crate) fn resume_downloads(&self) {
        let list = self.resume.lock().take();
        if let Some(list) = list {
            for (hash, kind) in list {
                let _ = self.fetch_tx.try_send(FetchReq::Resume { hash, kind });
            }
        }
    }

    /// Startup notices, once, as toasts.
    pub(crate) fn flush_notices(&self) {
        let notices: Vec<String> = std::mem::take(&mut *self.notices.lock());
        if notices.is_empty() {
            return;
        }
        let events = self.events.clone();
        tokio::spawn(async move {
            // Give the UI a moment to register its listeners.
            tokio::time::sleep(Duration::from_millis(500)).await;
            for n in notices {
                events.toast(ToastKind::Info, n);
            }
        });
    }

    /// Refresh every DM summary (online states follow rosters and DM links).
    pub(crate) fn refresh_dms(&self) {
        for conv in self.net.convs() {
            if self.view(&conv).is_some_and(|v| v.is_dm) {
                self.refresh(&conv, Refresh { dm: true, ..Refresh::default() });
            }
        }
    }

    /// Test hook backing: loopback addresses of the endpoint's IPv4 sockets.
    pub(crate) fn loopback_addrs(&self) -> Vec<SocketAddr> {
        self.net
            .endpoint()
            .bound_sockets()
            .into_iter()
            .filter(|s| s.is_ipv4())
            .map(|s| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), s.port()))
            .collect()
    }

    /// Emit `toast`.
    pub(crate) fn toast(&self, kind: ToastKind, text: impl Into<String>) { self.events.toast(kind, text); }

    /// Emit `message-added` / `message-updated` for one stored message.
    pub(crate) fn emit_message(&self, ev: Ev, conv: &ConvId, stored: &tincan_core::event::StoredEvent) {
        let Some(view) = self.view(conv) else { return };
        let delivered = if view.is_dm { self.net.dm_delivered(conv) } else { 0 };
        if let Some(m) = self.message_dto(&view, stored, delivered) {
            self.events.emit(ev, m);
        }
    }
}

/// User-facing text for a start failure.
fn start_err(e: NetError) -> String {
    tracing::error!(error = %e, "starting the network failed");
    "Couldn't start networking. Check your connection and restart Tincan.".to_string()
}

/// Map a network error to the user-facing string.
pub(crate) fn net_err(e: NetError) -> String {
    match e {
        NetError::Core(c) => {
            tracing::warn!(error = %c, "core error");
            "Something went wrong. Try again.".to_string()
        }
        NetError::Endpoint(m) => {
            tracing::warn!(error = %m, "endpoint error");
            "Network error. Check your connection and try again.".to_string()
        }
        other => other.to_string(),
    }
}

/// Server, channel and display names: 1 to 100 characters, no control characters.
pub(crate) fn validate_name(name: &str, what: &str) -> Result<(), String> {
    if tincan_core::state::validate_name(name).is_err() {
        return Err(format!("{what} of 1 to {} characters.", tincan_core::state::MAX_NAME_CHARS));
    }
    Ok(())
}
