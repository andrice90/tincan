//! State shared by the `Net` handle, the session actors and the DM machinery.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use iroh::Endpoint;
use iroh::address_lookup::memory::MemoryLookup;
use iroh::protocol::Router;
use parking_lot::{Mutex, RwLock};
use tincan_core::election::Status;
use tincan_core::hlc::Hlc;
use tincan_core::keys::Identity;
use tincan_core::state::Change;
use tincan_core::event::StoredEvent;
use tincan_core::storage::Storage;
use tincan_core::wire::{AddrHint, ClipInfo, Ctl};
use tincan_core::{BlobHash, ConvId, ServerId, UserId};
use tokio::sync::broadcast;

use crate::addr::{endpoint_addr, merge_hint, my_hint};
use crate::config::NetConfig;
use crate::dm::DmManager;
use crate::events::NetEvent;
use crate::hooks::{CtlSource, ExtraCtlHandler};
use crate::link::Tasks;
use crate::media::MediaHub;
use crate::power;
use crate::rdv::Rendezvous;
use crate::replica::ReplicaRef;
use crate::session::{SessionCmd, SessionHandle};
use crate::timing::Timings;

/// Capacity of the event broadcast channel.
pub(crate) const EVENT_CAPACITY: usize = 4096;

/// Test hook that parks every session actor (a hung process: connections stay open, nothing
/// is processed or sent). Outside tests `wait_frozen` never resolves.
#[derive(Debug, Default)]
pub(crate) struct Freeze {
    #[cfg(test)]
    tx: tokio::sync::watch::Sender<bool>,
}

impl Freeze {
    /// Park every session actor.
    #[cfg(test)]
    pub fn freeze(&self) { self.tx.send_replace(true); }

    /// Resolves once frozen (never outside tests).
    pub async fn wait_frozen(&self) {
        #[cfg(test)]
        {
            let mut rx = self.tx.subscribe();
            if rx.wait_for(|v| *v).await.is_ok() {
                return;
            }
        }
        std::future::pending::<()>().await
    }
}

/// Test hooks that make a node misbehave the way a modified client would (tests only).
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct TestHooks {
    /// Host: put this into every `Welcome.your_next_seq` (0 = honest).
    pub welcome_next_seq: std::sync::atomic::AtomicU64,
    /// Host: store and fan out every member Publish, but answer it with this refusal.
    pub refuse_publish: parking_lot::Mutex<Option<String>>,
    /// Never promote to host (a successor that answers "not hosting" forever).
    pub never_promote: AtomicBool,
    /// History sync tasks started (host side, any link).
    pub syncs_started: std::sync::atomic::AtomicUsize,
}

/// Most known addresses kept (LAN sightings of strangers cannot grow it beyond this).
pub(crate) const MAX_HINTS: usize = 4096;

/// What one server's host is told I hold (`Net::set_announced`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Announced {
    /// Complete blobs (attachments and clips) that server's members may fetch from me.
    pub have: Vec<BlobHash>,
    /// Clips of my library announced there (their hashes count as held).
    pub clips: Vec<ClipInfo>,
}

/// User-controlled presence fields and held blobs.
#[derive(Debug, Default)]
pub(crate) struct Local {
    /// Only voice, muted, deafened and sharing are meaningful here; the rest is filled in
    /// by [`Inner::status`].
    pub status: Status,
    /// Complete blobs held (attachments and clips).
    pub have: BTreeSet<BlobHash>,
    /// Soundboard library.
    pub clips: Vec<ClipInfo>,
    /// Per-server announcements (`Net::set_announced`). Once set, each server's host learns
    /// only its own entry (none for a server missing from it), and `have` and `clips` above
    /// are no longer announced.
    pub scoped: Option<HashMap<ServerId, Announced>>,
}

/// Everything shared across tasks.
pub(crate) struct Inner {
    /// Signing identity (same key as the endpoint).
    pub identity: Identity,
    /// `identity.user()`.
    pub me: UserId,
    /// The iroh endpoint.
    pub endpoint: Endpoint,
    /// Persistence.
    pub storage: Arc<dyn Storage>,
    /// Configuration (display name and allow_hosting may change at runtime).
    pub cfg: RwLock<NetConfig>,
    /// Timers.
    pub timings: Timings,
    /// Clock for authored events.
    pub hlc: Hlc,
    /// Event fan-out to the app.
    pub events: broadcast::Sender<NetEvent>,
    /// Every loaded conversation.
    pub replicas: RwLock<HashMap<ConvId, ReplicaRef>>,
    /// One session per server.
    pub sessions: Mutex<HashMap<ServerId, SessionHandle>>,
    /// Known addresses.
    pub hints: Mutex<HashMap<UserId, AddrHint>>,
    /// The same addresses, visible to every iroh dial (blobs included).
    pub memory: MemoryLookup,
    /// Presence and held blobs.
    pub local: Mutex<Local>,
    /// DM machinery.
    pub dm: DmManager,
    /// App tap for media and call control messages (after the media layer saw them).
    pub extra: RwLock<Option<Arc<dyn ExtraCtlHandler>>>,
    /// Voice rooms, host SFUs, screen share and calls.
    pub media: MediaHub,
    /// Rendezvous record client (None when the layer is off).
    pub rdv: Option<Arc<Rendezvous>>,
    /// Where short code records live (None: short codes are off).
    pub codes: Option<Arc<crate::codes::CodeDir>>,
    /// Keys derived from short codes (bounded).
    pub code_keys: Mutex<crate::codes::KeyCache>,
    /// My friend code and its publisher.
    pub friend: Mutex<Option<(tincan_core::code::ShortCode, tokio::task::AbortHandle)>>,
    /// Serializes invite code creation and rotation.
    pub code_ops: tokio::sync::Mutex<()>,
    /// Every spawned task.
    pub tasks: Arc<Tasks>,
    /// The router (taken on shutdown).
    pub router: Mutex<Option<Router>>,
    /// Set once shutdown started.
    pub closed: AtomicBool,
    /// Test hook (see [`Freeze`]).
    pub freeze: Freeze,
    /// Test hooks (see [`TestHooks`]).
    #[cfg(test)]
    pub test: TestHooks,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "Net({:?})", self.me) }
}

impl Inner {
    /// Send an event to subscribers (none listening is fine).
    pub fn emit(&self, ev: NetEvent) { let _ = self.events.send(ev); }

    /// Emit `EventAdded` (and `ConvUpdated` for non-message changes) for applied events.
    pub fn emit_applied(&self, conv: ConvId, applied: &[(StoredEvent, Change)]) {
        for (ev, change) in applied {
            if !matches!(change, Change::Message { .. } | Change::Nothing) {
                self.emit(NetEvent::ConvUpdated { conv, change: change.clone() });
            }
            self.emit(NetEvent::EventAdded { conv, event: Arc::new(ev.clone()), change: change.clone() });
        }
    }

    /// The status reported to `server`'s host: like [`Inner::status`], but the voice room and
    /// sharing flag only when the room belongs to that server (a `ChannelId` means nothing
    /// in another server).
    pub fn status_for(&self, server: &ServerId, synced: bool) -> Status {
        let mut st = self.status(synced);
        if let Some(ch) = st.voice {
            let here = self.replica(server).is_some_and(|r| r.lock().state.channels.contains_key(&ch));
            if !here {
                st.voice = None;
                st.sharing = false;
            }
        }
        st
    }

    /// Change the user-controlled status; re-announced to every host when it changed.
    pub fn set_status(&self, f: impl FnOnce(&mut Status)) {
        let changed = {
            let mut local = self.local.lock();
            let before = local.status.clone();
            f(&mut local.status);
            local.status != before
        };
        if changed {
            for s in self.sessions.lock().values() {
                let _ = s.tx.try_send(SessionCmd::StatusChanged);
            }
            self.media.status_changed();
        }
    }

    /// The full status to report (`synced` as given).
    pub fn status(&self, synced: bool) -> Status {
        let cfg = self.cfg.read();
        let local = self.local.lock();
        Status {
            app_version: cfg.app_version.clone(),
            uptime_s: power::uptime_s(),
            ac_power: power::ac_power(),
            voice: local.status.voice,
            muted: local.status.muted,
            deafened: local.status.deafened,
            sharing: local.status.sharing,
            hosting_ok: cfg.allow_hosting,
            synced,
        }
    }

    /// Held blobs announced to `server`'s host: attachments plus clip hashes, deduplicated.
    pub fn have_list_for(&self, server: &ServerId) -> Vec<BlobHash> {
        let local = self.local.lock();
        let (have, clips): (&[BlobHash], &[ClipInfo]) = match &local.scoped {
            Some(m) => match m.get(server) {
                Some(a) => (&a.have, &a.clips),
                None => (&[], &[]),
            },
            None => {
                let mut set = local.have.clone();
                set.extend(local.clips.iter().map(|c| c.hash));
                return set.into_iter().collect();
            }
        };
        let set: BTreeSet<BlobHash> = have.iter().copied().chain(clips.iter().map(|c| c.hash)).collect();
        set.into_iter().collect()
    }

    /// Do I hold `hash` (blob or clip) as far as `server`'s members are told?
    pub fn holds_for(&self, server: &ServerId, hash: &BlobHash) -> bool {
        let local = self.local.lock();
        match &local.scoped {
            Some(m) => m.get(server).is_some_and(|a| a.have.contains(hash) || a.clips.iter().any(|c| c.hash == *hash)),
            None => local.have.contains(hash) || local.clips.iter().any(|c| c.hash == *hash),
        }
    }

    /// Soundboard clips announced to `server`'s host.
    pub fn clips_for(&self, server: &ServerId) -> Vec<ClipInfo> {
        let local = self.local.lock();
        match &local.scoped {
            Some(m) => m.get(server).map(|a| a.clips.clone()).unwrap_or_default(),
            None => local.clips.clone(),
        }
    }

    /// Allow hosting setting.
    pub fn allow_hosting(&self) -> bool { self.cfg.read().allow_hosting }

    /// Known address of `user`.
    pub fn hint(&self, user: &UserId) -> Option<AddrHint> { self.hints.lock().get(user).cloned() }

    /// Remember an address (merged with what is known) for app dials and iroh lookups.
    /// Returns true if something new was learned.
    pub fn learn_hint(&self, user: &UserId, hint: &AddrHint) -> bool {
        if *user == self.me || (hint.relay.is_none() && hint.direct.is_empty()) {
            return false;
        }
        let hint = hint.bounded();
        let merged = {
            let mut hints = self.hints.lock();
            if hints.len() >= MAX_HINTS && !hints.contains_key(user) {
                return false;
            }
            let entry = hints.entry(*user).or_default();
            let merged = merge_hint(entry, &hint);
            if merged == *entry {
                return false;
            }
            *entry = merged.clone();
            merged
        };
        if let Some(addr) = endpoint_addr(user, Some(&merged)) {
            self.memory.add_endpoint_info(addr);
        }
        true
    }

    /// This device's address.
    pub fn my_hint(&self) -> AddrHint { my_hint(&self.endpoint) }

    /// A media/call control message: the media layer handles it, then the app's tap (if
    /// registered) sees it too.
    pub fn extra(self: &Arc<Self>, source: CtlSource, ctl: Ctl) {
        self.media.on_ctl(self, source, &ctl);
        let handler = self.extra.read().clone();
        if let Some(h) = handler {
            h.handle(source, ctl);
        }
    }

    /// Replica of a conversation.
    pub fn replica(&self, conv: &ConvId) -> Option<ReplicaRef> { self.replicas.read().get(conv).cloned() }

    /// Display name of `user` from any server I share with them.
    pub fn name_from_servers(&self, user: &UserId) -> Option<String> {
        let reps: Vec<ReplicaRef> = self.replicas.read().values().cloned().collect();
        for r in reps {
            let r = r.lock();
            if !r.state.is_dm
                && let Some(m) = r.state.members.get(user)
                && !m.name.is_empty()
            {
                return Some(m.name.clone());
            }
        }
        None
    }

    /// Is `user` a member of one of my servers?
    pub fn shares_server(&self, user: &UserId) -> bool {
        let reps: Vec<ReplicaRef> = self.replicas.read().values().cloned().collect();
        reps.iter().any(|r| {
            let r = r.lock();
            !r.state.is_dm && r.state.is_member(user)
        })
    }

    /// Is `user` a member of one of my servers or a DM contact (a stored DM conversation)?
    pub fn is_known_peer(&self, user: &UserId) -> bool {
        if *user == self.me {
            return false;
        }
        self.replica(&ConvId::dm(&self.me, user)).is_some() || self.shares_server(user)
    }

    /// Does `user` get a pre-auth slot from the known pool (`proto::PreAuth`)? A member of
    /// one of my servers, or a DM contact I wrote to. A stranger whose message opened a
    /// conversation I never answered is still unknown here: one valid message must not buy
    /// slots in the pool that members and contacts rely on.
    pub fn is_trusted_dialer(&self, user: &UserId) -> bool {
        if *user == self.me {
            return false;
        }
        if self.shares_server(user) {
            return true;
        }
        self.replica(&ConvId::dm(&self.me, user)).is_some_and(|r| r.lock().state.head(&self.me) > 0)
    }

    /// Is `user` online in any server roster I know of, or connected over a DM?
    pub fn peer_online(&self, user: &UserId) -> bool {
        if self.dm.is_connected(user) {
            return true;
        }
        let sessions = self.sessions.lock();
        sessions.values().any(|s| s.shared.roster.lock().as_ref().is_some_and(|r| r.online.iter().any(|e| e.user == *user)))
    }
}
