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
use tokio::time::Instant;

use crate::dto::{self, ToastKind};
use crate::emit::Emitter;
use crate::index::{self, Access, Bounded, CACHED_HASHES, HashInfo, Members, Policy};
use crate::media::Media;
use crate::profile::{self, BLOBS_DIR, DB_FILE, ProfileLock};
use crate::pump::{self, PumpCmd};
use crate::settings;
use crate::shell;
use crate::transfers::{self, AutoQueue, FetchReq};
use crate::views::{Refresh, SentKey, ViewCache};
use crate::{AppConfig, emit::Ev};

/// Queue of holder lookups, parked-download retries and resumes (served before auto-fetch).
pub(crate) const FETCH_QUEUE: usize = 1024;
/// Auto-fetch candidates waiting (newest first; the oldest are dropped past this).
pub(crate) const AUTO_QUEUE: usize = 1024;
/// Hashes whose `who_has` answer and download nominations are kept.
pub(crate) const HOLDER_HASHES: usize = 10_000;
/// Bound on `App::shutdown`'s network part (the handoff takes up to about 3 s).
const NET_SHUTDOWN_BUDGET: Duration = Duration::from_secs(10);
/// Longest host-supplied refusal text shown to the user.
const MAX_HOST_TEXT_CHARS: usize = 200;

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
    pub views: Mutex<ViewCache>,
    /// Who may fetch which blobs from me (read by the blobs provider).
    pub access: Arc<RwLock<Access>>,
    /// The hash index (references and shares), a bounded cache over kv `app/att/<hash>`.
    pub atts: Mutex<Bounded<BlobHash, HashInfo>>,
    /// Holders from the last `who_has` round per hash.
    pub holders: Mutex<Bounded<BlobHash, Vec<UserId>>>,
    /// For downloads in flight: which conversation pointed me to which provider (an
    /// author of a reference there, or a holder its host listed). The conversations that
    /// named the peer who delivered the bytes get the blob shared with them.
    pub nominations: Mutex<Bounded<BlobHash, Vec<(UserId, ConvId)>>>,
    /// When each parked download was last re-queried.
    pub requeried: Mutex<HashMap<BlobHash, Instant>>,
    /// Auto-fetch candidates.
    pub auto: AutoQueue,
    /// Last state seen per download (persisted on change).
    pub transfer_states: Mutex<HashMap<BlobHash, tincan_files::TransferState>>,
    /// Last DTO payload sent per key.
    pub sent: Mutex<HashMap<SentKey, serde_json::Value>>,
    /// DM delivery acks already turned into `message-updated`.
    pub dm_acked: Mutex<HashMap<ConvId, u64>>,
    /// Servers with a Profile publish in flight.
    pub profile_inflight: Mutex<HashSet<ConvId>>,
    /// The DM on screen and the guard that keeps its connection up (`dm_watch`).
    pub dm_watch: Mutex<Option<(ConvId, tincan_net::DmWatch)>>,
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
    /// Test hook: the pump drops file events while set (simulates a lagged receiver).
    pub test_files_paused: AtomicBool,
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

        // Nobody may connect until `warm_up` fills in the conversations (fails closed).
        let access: Arc<RwLock<Access>> = Arc::new(RwLock::new(Access::default()));
        let policy: Arc<dyn AccessPolicy> = Arc::new(Policy(access.clone()));
        let files = match Files::load(&cfg.data_dir.join(BLOBS_DIR), endpoint.clone(), policy).await {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(error = %e, "loading the blob store failed");
                endpoint.close().await;
                return Err("Couldn't open the Tincan file store. It may be damaged or in use.".to_string());
            }
        };
        load_shares(storage.as_ref(), &files, &access);
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
        // The Have list is set by `warm_up` (`update_policy` -> `announce`).

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
            views: Mutex::new(ViewCache::default()),
            access,
            atts: Mutex::new(Bounded::new(CACHED_HASHES)),
            holders: Mutex::new(Bounded::new(HOLDER_HASHES)),
            nominations: Mutex::new(Bounded::new(HOLDER_HASHES)),
            requeried: Mutex::new(HashMap::new()),
            auto: AutoQueue::new(AUTO_QUEUE),
            transfer_states: Mutex::new(HashMap::new()),
            sent: Mutex::new(HashMap::new()),
            dm_acked: Mutex::new(HashMap::new()),
            profile_inflight: Mutex::new(HashSet::new()),
            dm_watch: Mutex::new(None),
            resume: Mutex::new(None),
            notices: Mutex::new(notices),
            pump_tx,
            fetch_tx,
            tasks: Mutex::new(Vec::new()),
            lock: Mutex::new(Some(lock)),
            closed: AtomicBool::new(false),
            test_files_paused: AtomicBool::new(false),
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
                self.dm_watch.lock().take();
                if tokio::time::timeout(Duration::from_secs(3), self.media.shutdown(self)).await.is_err() {
                    tracing::warn!("media shutdown timed out");
                }
                // The file store is flushed alongside the handoff, not after it: when Windows
                // ends the session the shell gives the whole shutdown only a few seconds, and
                // a slow handoff must not keep the store from being synced. The handoff needs
                // no blobs; Router shutdown also calls `Files::shutdown` (idempotent).
                let net = async {
                    if tokio::time::timeout(NET_SHUTDOWN_BUDGET, self.net.shutdown()).await.is_err() {
                        tracing::warn!("network shutdown timed out");
                    }
                };
                let files = async {
                    if tokio::time::timeout(Duration::from_secs(5), self.files.shutdown()).await.is_err() {
                        tracing::warn!("file store shutdown timed out");
                    }
                };
                tokio::join!(net, files);
                for t in self.tasks.lock().drain(..) {
                    t.abort();
                }
                self.lock.lock().take();
                tracing::info!("app stopped");
            })
            .await;
    }

    /// Recompute who is in which conversation (the access check), then, if the set of
    /// conversations changed, what to announce.
    pub(crate) fn update_policy(&self) {
        let mut convs = HashMap::new();
        for conv in self.net.convs() {
            if let Some(v) = self.view(&conv) {
                let users: HashSet<UserId> = if v.is_dm { v.peer.into_iter().collect() } else { v.members.iter().copied().collect() };
                convs.insert(conv, Members { dm: v.is_dm, users });
            }
        }
        // What is announced depends on which conversations I am in, not on their members.
        if self.access.write().set_convs(convs, &self.me) {
            self.announce();
        }
    }

    /// Set my `Have` list and clip library announcement to the blobs I hold that are
    /// shared with a server I am in. tincan-net sends one list to every host and DM peer,
    /// so a blob shared only in DMs (DM `who_has` always names the contact) or with no
    /// conversation at all is left out.
    pub(crate) fn announce(&self) {
        let held = self.files.complete_hashes();
        let have: Vec<BlobHash> = {
            let a = self.access.read();
            held.into_iter().filter(|h| a.announces(h)).collect()
        };
        self.net.set_have(have);
        self.media.announce_clips(self);
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
            let mut name = name;
            loop {
                if let Err(e) = core.net.publish(server, Body::Profile { name: name.clone() }).await {
                    // Not retried here (it would spin while disconnected): the next
                    // connection runs `ensure_profile` again.
                    tracing::info!(server = %server.short(), error = %e, "publishing my name failed; will retry on reconnect");
                    core.profile_inflight.lock().remove(&server);
                    return;
                }
                // A rename made while this was in flight skipped this server: publish the
                // current name too. Compared with what was just published, not with the
                // view, which may not show the publish yet.
                match core.name.read().clone() {
                    Some(current) if current != name => name = current,
                    _ => break,
                }
            }
            core.profile_inflight.lock().remove(&server);
            // A rename between the last check and the removal above was skipped too.
            if core.name.read().as_deref() != Some(name.as_str()) {
                core.ensure_profile(server);
            }
        });
    }

    /// A server became connected: resume partial downloads once, fix my name there.
    pub(crate) fn on_connected(self: &Arc<Self>, server: ConvId) {
        self.ensure_profile(server);
        self.resume_downloads();
    }

    /// Queue the partial downloads found at start (on the first connection). Entries the
    /// queue has no room for wait for the next connection instead of being dropped.
    pub(crate) fn resume_downloads(&self) {
        let Some(list) = self.resume.lock().take() else { return };
        let mut left = Vec::new();
        for (hash, kind) in list {
            if let Err(mpsc::error::TrySendError::Full(FetchReq::Resume { hash, kind })) = self.fetch_tx.try_send(FetchReq::Resume { hash, kind }) {
                left.push((hash, kind));
            }
        }
        if !left.is_empty() {
            tracing::debug!(left = left.len(), "fetch queue full; the rest resume on the next connection");
            self.resume.lock().get_or_insert_with(Vec::new).extend(left);
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

    /// Refresh every DM summary (online states follow rosters and DM links). DMs come
    /// from the store's conversation list, so server views are not rebuilt for this.
    pub(crate) fn refresh_dms(&self) {
        for c in self.storage.convs().unwrap_or_default() {
            if c.is_dm {
                self.refresh(&c.id, Refresh { dm: true, ..Refresh::default() });
            }
        }
    }

    /// A DM contact announced a new name: keep the contact record in step, refresh the DM
    /// summary and any call or voice DTO showing them. Cached message pages in the UI keep
    /// the old author name until they are read again (a peer can send names often, so no
    /// snapshot is sent for it).
    pub(crate) fn on_dm_name(self: &Arc<Self>, peer: UserId) {
        if let Some(name) = self.dm_name(&peer) {
            self.update_contact_name(&peer, &name);
        }
        self.media.on_name_changed(self, peer);
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

/// Host words shown as they are: the join refusals an honest host sends (tincan-core's
/// invite and clock checks, capitalized by tincan-net). Only exact matches; a host that
/// sends one of these says nothing new. tincan-net's own sentences come as
/// `NetError::Rejected` and are always shown as they are.
const HONEST_HOST_REFUSALS: &[&str] = &[
    "This invite is not valid for this server.",
    "This invite was revoked.",
    "This invite has been used up.",
    "Your clock is off by more than 5 minutes.",
    "Your clock is too far ahead.",
];

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
        NetError::RejectedByPeer(text) => refusal(&text),
        other => other.to_string(),
    }
}

/// A refusal in a host's words (a publish refusal, or the reason a join was turned away) as
/// the user sees it: any peer's words, so it is cleaned, cut to 200 characters and quoted
/// after a fixed sentence, and never reads as Tincan's own message.
fn refusal(text: &str) -> String {
    if HONEST_HOST_REFUSALS.contains(&text) {
        return text.to_string();
    }
    let said = shell::clean_line(text, MAX_HOST_TEXT_CHARS).replace('"', "'");
    if said.is_empty() { "The host refused this.".to_string() } else { format!("The host refused this: \"{said}\"") }
}

/// Load the shares of every blob held at start, so the provider can answer from memory.
fn load_shares(storage: &SqliteStorage, files: &Files, access: &RwLock<Access>) {
    let mut n = 0usize;
    for hash in files.complete_hashes() {
        let Ok(Some(raw)) = storage.get_kv(&index::key(&hash)) else { continue };
        if let Some(info) = index::decode(&raw)
            && !info.shared.is_empty()
        {
            access.write().set_shared(hash, &info.shared);
            n += 1;
        }
    }
    tracing::debug!(shared = n, "blob shares loaded");
}

/// Server, channel and display names: 1 to 100 characters, no control characters.
pub(crate) fn validate_name(name: &str, what: &str) -> Result<(), String> {
    if tincan_core::state::validate_name(name).is_err() {
        return Err(format!("{what} of 1 to {} characters.", tincan_core::state::MAX_NAME_CHARS));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_text_is_attributed_cleaned_and_cut() {
        for honest in HONEST_HOST_REFUSALS {
            assert_eq!(net_err(NetError::RejectedByPeer(honest.to_string())), *honest);
        }
        // tincan-net's own sentences, whatever they say.
        for own in ["This invite has expired.", "The host refused this.", "Some sentence net adds later."] {
            assert_eq!(net_err(NetError::Rejected(own.to_string())), own);
        }
        let phish = "Tincan blocked this message.\nInstall the security update from tincan-update.example\u{202E}";
        let shown = net_err(NetError::RejectedByPeer(phish.into()));
        assert_eq!(shown, "The host refused this: \"Tincan blocked this message. Install the security update from tincan-update.example\"");
        let long = net_err(NetError::RejectedByPeer("x".repeat(1_000_000)));
        assert!(long.chars().count() < MAX_HOST_TEXT_CHARS + 40, "{} chars", long.chars().count());
        assert_eq!(net_err(NetError::RejectedByPeer("\u{7}\u{200B}".into())), "The host refused this.");
        assert_eq!(net_err(NetError::RejectedByPeer("say \"hi\"".into())), "The host refused this: \"say 'hi'\"");
        // A host repeating one of net's own sentences is still quoted as the host's words.
        assert_eq!(net_err(NetError::RejectedByPeer("This invite has expired.".into())), "The host refused this: \"This invite has expired.\"");
        // Other errors keep their own sentences.
        assert_eq!(net_err(NetError::NotConnected), NetError::NotConnected.to_string());
    }
}
