//! The public `Net` handle.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use iroh::Endpoint;
use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::Connection;
use iroh::protocol::{Router, RouterBuilder};
use n0_future::StreamExt;
use parking_lot::{Mutex, RwLock};
use tincan_core::build::{self, HelloInfo};
use tincan_core::election::Status;
use tincan_core::event::{Body, StoredEvent};
use tincan_core::hlc::Hlc;
use tincan_core::invite::{Invite, invite_secret};
use tincan_core::keys::{Identity, random_bytes};
use tincan_core::state::{ConvState, dm_channel};
use tincan_core::storage::{ConvRecord, PeerRecord, Storage};
use tincan_core::wire::{AddrHint, ClipInfo, Ctl, RejectCode, Roster};
use tincan_core::{ALPN_DM, ALPN_SERVER, BlobHash, ChannelId, ConvId, PROTOCOL_VERSION, ServerId, UserId, now_ms};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::addr::hint_from_addr;
use crate::config::{NetConfig, take_mdns};
use crate::dm::{self, DmHold, DmManager};
use crate::error::{NetError, Result, capitalize};
use crate::events::{ConnectionInfo, NetEvent};
use crate::hooks::ExtraCtlHandler;
use crate::inner::{EVENT_CAPACITY, Inner, Local};
use crate::link::{self, Dialed, Tasks};
use crate::power;
use crate::proto::{DmProto, ServerProto};
use crate::replica::{Replica, store_floor, stored_floor};
use crate::session::{self, CtlTarget, SessionCmd, StartMode};
use crate::timing::Timings;

/// The network layer: one per profile. Cheap to clone.
///
/// Created with [`Net::start`] on an endpoint from [`crate::build_endpoint`]. Every method is
/// safe to call from any task. Events arrive on [`Net::subscribe`].
#[derive(Clone, Debug)]
pub struct Net {
    inner: Arc<Inner>,
}

const NO_ONE_REACHABLE: &str = "Couldn't reach anyone in that server. Check the invite or try again later.";

impl Net {
    /// Start the network layer.
    ///
    /// Spawns the iroh `Router` with `ALPN_SERVER`, `ALPN_DM` and whatever `extra` registers
    /// (the app adds the blobs protocol), loads every stored conversation, rebuilds its
    /// state from storage, starts one session per server (cold open: dial the last host,
    /// successors and recently seen members; solo host after 6 s) and resumes undelivered
    /// DMs. `identity` must be the endpoint's key.
    pub async fn start(
        endpoint: Endpoint,
        storage: Arc<dyn Storage>,
        identity: Identity,
        cfg: NetConfig,
        extra: impl FnOnce(RouterBuilder) -> RouterBuilder,
    ) -> Result<Net> {
        Self::start_with(endpoint, storage, identity, cfg, extra, Timings::default()).await
    }

    /// `start` with explicit timings (tests shrink them).
    pub(crate) async fn start_with(
        endpoint: Endpoint,
        storage: Arc<dyn Storage>,
        identity: Identity,
        cfg: NetConfig,
        extra: impl FnOnce(RouterBuilder) -> RouterBuilder,
        timings: Timings,
    ) -> Result<Net> {
        power::mark_start();
        if endpoint.id() != identity.secret().public() {
            return Err(NetError::Endpoint("the endpoint was built with a different key".into()));
        }
        let me = identity.user();
        let memory = MemoryLookup::new();
        endpoint.address_lookup().map_err(|e| NetError::Endpoint(e.to_string()))?.add(memory.clone());

        let mut replicas = HashMap::new();
        let mut hlc_floor = 0u64;
        for record in storage.convs()? {
            let id = record.id;
            match Replica::load(storage.as_ref(), record, &me) {
                Ok(r) => {
                    hlc_floor = hlc_floor.max(r.state.heads.get(&me).map(|h| h.hlc).unwrap_or(0));
                    replicas.insert(id, Arc::new(Mutex::new(r)));
                }
                Err(e) => tracing::error!(conv = %id.short(), error = %e, "loading conversation failed"),
            }
        }
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let rdv_client = cfg
            .rendezvous_enabled()
            .then(|| Arc::new(crate::rdv::Rendezvous::new(&endpoint, cfg.relay == crate::config::RelayChoice::N0Default)));
        let inner = Arc::new(Inner {
            identity,
            me,
            endpoint: endpoint.clone(),
            storage,
            cfg: RwLock::new(cfg),
            timings,
            hlc: Hlc::with_floor(hlc_floor),
            events,
            replicas: RwLock::new(replicas),
            sessions: Mutex::new(HashMap::new()),
            hints: Mutex::new(HashMap::new()),
            memory,
            local: Mutex::new(Local::default()),
            dm: DmManager::default(),
            extra: RwLock::new(None),
            media: crate::media::MediaHub::default(),
            rdv: rdv_client,
            tasks: Arc::new(Tasks::default()),
            router: Mutex::new(None),
            closed: std::sync::atomic::AtomicBool::new(false),
            freeze: crate::inner::Freeze::default(),
        });
        let weak = Arc::downgrade(&inner);
        let builder = Router::builder(endpoint.clone())
            .accept(ALPN_SERVER, ServerProto { net: weak.clone() })
            .accept(ALPN_DM, DmProto { net: weak });
        let router = extra(builder).spawn();
        *inner.router.lock() = Some(router);

        let reps: Vec<(ConvId, Arc<Mutex<Replica>>)> = inner.replicas.read().iter().map(|(k, v)| (*k, v.clone())).collect();
        let mut dm_peers = Vec::new();
        for (id, rep) in reps {
            let (is_dm, peer) = {
                let r = rep.lock();
                (r.record.is_dm, r.record.peer)
            };
            if is_dm {
                dm_peers.extend(peer);
            } else {
                let handle = session::spawn(&inner, id, rep, StartMode::ColdOpen);
                inner.sessions.lock().insert(id, handle);
            }
        }
        inner.dm.peers_seen(&inner, &dm_peers);
        if let Some(mdns) = take_mdns(&endpoint.id()) {
            let net = inner.clone();
            inner.tasks.spawn(async move {
                let mut stream = mdns.subscribe().await;
                while let Some(ev) = stream.next().await {
                    if let iroh_mdns_address_lookup::DiscoveryEvent::Discovered { endpoint_info, .. } = ev {
                        let user = UserId::from_public_key(&endpoint_info.endpoint_id);
                        let hint = hint_from_addr(&endpoint_info.to_endpoint_addr());
                        net.learn_hint(&user, &hint);
                        wake(&net, user);
                    }
                }
                drop(mdns);
            });
        }
        tracing::info!(me = %me.short(), "network started");
        Ok(Net { inner })
    }

    /// Subscribe to network events.
    pub fn subscribe(&self) -> broadcast::Receiver<NetEvent> { self.inner.events.subscribe() }

    /// The local user id.
    pub fn me(&self) -> UserId { self.inner.me }

    /// The endpoint (shared with tincan-files).
    pub fn endpoint(&self) -> &Endpoint { &self.inner.endpoint }

    /// This device's current address hint.
    pub fn my_addr(&self) -> AddrHint { self.inner.my_hint() }

    /// Remember an address for `user` (invite bootstrap, pasted contact, tests) and retry any
    /// session or DM that is waiting for that user.
    pub fn add_addr_hint(&self, user: UserId, hint: AddrHint) {
        self.inner.learn_hint(&user, &hint);
        wake(&self.inner, user);
    }

    /// Display name used for new servers and joins.
    pub fn set_display_name(&self, name: &str) { self.inner.cfg.write().display_name = name.trim().to_string(); }

    /// Change the "allow hosting" setting (takes effect on the next election).
    pub fn set_allow_hosting(&self, allow: bool) {
        self.inner.cfg.write().allow_hosting = allow;
        self.notify_sessions(|| SessionCmd::StatusChanged);
    }

    /// Create a server owned by me (Genesis, my Profile, "general" and "Lounge") and host it.
    pub async fn create_server(&self, name: &str) -> Result<ServerId> {
        let inner = &self.inner;
        let owner = inner.cfg.read().display_name.clone();
        let ns = build::new_server(&inner.identity, name, &owner, &inner.hlc).map_err(NetError::from_core_user)?;
        let server = ns.server;
        let record = ConvRecord {
            id: server,
            is_dm: false,
            peer: None,
            name: name.trim().to_string(),
            server_secret: Some(ns.server_secret),
            last_epoch: 0,
            last_host: Some(inner.me),
            joined_ms: now_ms(),
        };
        inner.storage.upsert_conv(&record)?;
        let mut replica = Replica::new(inner.storage.as_ref(), record, ConvState::new_server(server));
        let mut applied = Vec::new();
        for signed in ns.events {
            let ev = StoredEvent::from_signed(signed)?;
            applied.extend(replica.ingest(inner.storage.as_ref(), &inner.hlc, ev).applied);
        }
        replica.reserve_own_seq(inner.storage.as_ref(), ns.next_seq - 1);
        let rep = Arc::new(Mutex::new(replica));
        inner.replicas.write().insert(server, rep.clone());
        inner.emit_applied(server, &applied);
        let handle = session::spawn(inner, server, rep, StartMode::NewServer);
        inner.sessions.lock().insert(server, handle);
        inner.emit(NetEvent::Joined { server });
        Ok(server)
    }

    /// Create an invite code for `server`: valid 7 days, unlimited uses, bootstrap = me and
    /// the current host with their addresses. The `InviteCreate` event is published first.
    pub async fn create_invite(&self, server: ServerId) -> Result<String> {
        let inner = &self.inner;
        let rep = inner.replica(&server).ok_or(NetError::UnknownConv)?;
        let (keys, name) = {
            let r = rep.lock();
            (r.keys.clone().ok_or(NetError::UnknownConv)?, r.state.name.clone())
        };
        let seed: [u8; 16] = random_bytes();
        let invite_pk = *invite_secret(&seed, &server).public().as_bytes();
        let expires_ms = now_ms().saturating_add(build::DEFAULT_INVITE_TTL_MS);
        self.publish(server, Body::InviteCreate { invite_pk, expires_ms, max_uses: 0 }).await?;
        let mut bootstrap = vec![(inner.me, inner.my_hint())];
        if let Some(host) = self.connection(&server).host
            && host != inner.me
        {
            let addr = self
                .roster(&server)
                .and_then(|r| r.online.iter().find(|e| e.user == host).map(|e| e.addr.clone()))
                .or_else(|| inner.hint(&host))
                .unwrap_or_default();
            bootstrap.push((host, addr));
        }
        let invite = Invite {
            v: PROTOCOL_VERSION,
            server,
            name_hint: name,
            invite_seed: seed,
            rdv_pk: *keys.rdv_pk().as_bytes(),
            rdv_read_key: keys.rdv_read_key,
            bootstrap,
            expires_ms,
        };
        Ok(invite.encode())
    }

    /// Join a server with an invite code (or `tincan://join/` link). Dials the bootstrap
    /// peers, follows redirects to the host, sends the JoinRequest, stores what the host
    /// returns and starts the session on the same connection. Errors are user-presentable.
    /// Joining a server I am already in returns its id.
    pub async fn join(&self, invite: &str) -> Result<ServerId> {
        let inner = &self.inner;
        let invite = Invite::decode(invite).map_err(NetError::from_core_user)?;
        let server = invite.server;
        if inner.replica(&server).is_some() {
            return Ok(server);
        }
        if invite.is_expired(now_ms()) {
            return Err(NetError::Rejected("This invite has expired.".into()));
        }
        let name = inner.cfg.read().display_name.clone();
        let seq = stored_floor(inner.storage.as_ref(), &server).saturating_add(1);
        let hlc = inner.hlc.now();
        let budget = inner.timings.join_timeout;
        let attempt = tokio::time::timeout(budget, join_dial(inner, &invite, &name, seq, hlc)).await;
        let (host, dialed, profile) = match attempt {
            Ok(r) => r?,
            Err(_) => return Err(NetError::Unreachable(NO_ONE_REACHABLE.into())),
        };
        let acc = build::verify_join_accepted(&dialed.reply, &invite, &inner.me).map_err(NetError::from_core_user)?;
        let server_name = match &acc.genesis.event.body {
            Body::Genesis { name, .. } => name.clone(),
            _ => invite.name_hint.clone(),
        };
        let record = ConvRecord {
            id: server,
            is_dm: false,
            peer: None,
            name: server_name,
            server_secret: Some(acc.server_secret),
            last_epoch: 0,
            last_host: Some(host),
            joined_ms: now_ms(),
        };
        inner.storage.upsert_conv(&record)?;
        store_floor(inner.storage.as_ref(), &server, seq);
        let mut replica = Replica::new(inner.storage.as_ref(), record, ConvState::new_server(server));
        let mut applied = Vec::new();
        for ev in [acc.genesis.clone(), acc.admit.clone(), profile] {
            applied.extend(replica.ingest(inner.storage.as_ref(), &inner.hlc, ev).applied);
        }
        replica.reserve_own_seq(inner.storage.as_ref(), seq);
        if let Some(h) = inner.hint(&host)
            && let Err(e) = inner.storage.upsert_peer(&server, &PeerRecord { user: host, addr: h, last_seen_ms: now_ms() })
        {
            tracing::warn!(error = %e, "persisting host address failed");
        }
        let rep = Arc::new(Mutex::new(replica));
        inner.replicas.write().insert(server, rep.clone());
        inner.emit_applied(server, &applied);
        // Normal Hello on the same connection.
        let hello = build::hello(
            &inner.me,
            &server,
            &acc.keys.hello_key,
            &host,
            HelloInfo { epoch_seen: 0, status: inner.status_for(&server, false), have: inner.have_list(), clips: inner.clips() },
        );
        let Dialed { conn, send, reader, .. } = dialed;
        let mode = match link::continue_exchange(conn, send, reader, &hello, inner.timings.dial).await {
            Ok(d) if matches!(d.reply, Ctl::Welcome { .. }) => StartMode::Joined(Box::new(d)),
            Ok(d) => {
                link::close_conn(&d.conn, tincan_core::CLOSE_NORMAL, "unexpected reply");
                StartMode::ColdOpen
            }
            Err(e) => {
                tracing::info!(error = %e, "hello after join failed; cold open");
                StartMode::ColdOpen
            }
        };
        let handle = session::spawn(inner, server, rep, mode);
        inner.sessions.lock().insert(server, handle);
        inner.emit(NetEvent::Joined { server });
        tracing::info!(server = %server.short(), "joined");
        Ok(server)
    }

    /// Leave a server: end my voice session there, publish `MemberLeave` (best effort), hand
    /// off if hosting, delete the local copy and emit `Left`.
    pub async fn leave(&self, server: ServerId) -> Result<()> {
        let inner = &self.inner;
        let tx = inner.sessions.lock().get(&server).map(|s| s.tx.clone()).ok_or(NetError::UnknownConv)?;
        inner.media.leave_server(inner, server).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), self.publish(server, Body::MemberLeave { last_seq: 0 })).await;
        let (reply, rx) = oneshot::channel();
        if tx.send(SessionCmd::Stop { handoff: true, reply }).await.is_ok() {
            let _ = tokio::time::timeout(inner.timings.handoff_ack + Duration::from_secs(3), rx).await;
        }
        inner.sessions.lock().remove(&server);
        inner.replicas.write().remove(&server);
        inner.storage.delete_conv(&server)?;
        inner.emit(NetEvent::Left { server });
        Ok(())
    }

    /// Author an event in `conv` with my next seq and a fresh hlc.
    ///
    /// Servers: sent to the host and returned once it acknowledged (applied locally then);
    /// applied directly when hosting. A `MemberLeave` body gets its `last_seq` filled in.
    /// DMs: only `Message` (its channel is set to the DM channel) and `Delete`; stored at
    /// once and delivered when the contact is reachable (see `dm_delivered`).
    pub async fn publish(&self, conv: ConvId, body: Body) -> Result<StoredEvent> {
        if dm::peer_of(&self.inner, &conv).is_some() {
            return dm::publish(&self.inner, conv, body);
        }
        let tx = self.inner.sessions.lock().get(&conv).map(|s| s.tx.clone()).ok_or(NetError::UnknownConv)?;
        let (reply, rx) = oneshot::channel();
        tx.send(SessionCmd::Publish { body, reply }).await.map_err(|_| NetError::Closed)?;
        rx.await.map_err(|_| NetError::Closed)?
    }

    /// Snapshot of a conversation's state.
    pub fn conv_state(&self, conv: &ConvId) -> Option<ConvState> { self.inner.replica(conv).map(|r| r.lock().state.clone()) }

    /// Every loaded conversation (servers and DMs).
    pub fn convs(&self) -> Vec<ConvId> { self.inner.replicas.read().keys().copied().collect() }

    /// The latest roster of a server.
    pub fn roster(&self, server: &ServerId) -> Option<Roster> {
        self.inner.sessions.lock().get(server).and_then(|s| s.shared.roster.lock().clone())
    }

    /// Connection status of a server (default/Offline for unknown servers).
    pub fn connection(&self, server: &ServerId) -> ConnectionInfo {
        self.inner.sessions.lock().get(server).map(|s| s.shared.info.lock().clone()).unwrap_or_default()
    }

    /// Change my mute or deafen state (voice and sharing are set by `voice_join` and
    /// `share_start`); re-announced to every host (and a call peer) on change. The voice room
    /// is only reported to the server it belongs to.
    pub fn set_status(&self, f: impl FnOnce(&mut Status)) { self.inner.set_status(f); }

    /// My full status as reported to hosts.
    pub fn status(&self) -> Status { self.inner.status(true) }

    /// Replace the set of complete blobs I hold.
    pub fn set_have(&self, hashes: Vec<BlobHash>) { self.change_have(|l| l.have = hashes.into_iter().collect()); }

    /// One more complete blob.
    pub fn have_add(&self, hash: BlobHash) {
        self.change_have(|l| {
            l.have.insert(hash);
        });
    }

    /// Replace my soundboard library (its hashes count as held blobs).
    pub fn set_clips(&self, clips: Vec<ClipInfo>) { self.change_have(|l| l.clips = clips); }

    fn change_have(&self, f: impl FnOnce(&mut Local)) {
        let before: HashSet<BlobHash> = self.inner.have_list().into_iter().collect();
        f(&mut self.inner.local.lock());
        let after: HashSet<BlobHash> = self.inner.have_list().into_iter().collect();
        let add: Vec<BlobHash> = after.difference(&before).copied().collect();
        let remove: Vec<BlobHash> = before.difference(&after).copied().collect();
        if add.is_empty() && remove.is_empty() {
            return;
        }
        self.inner.dm.broadcast_have(&add, &remove);
        self.notify_sessions(|| SessionCmd::HaveChanged { add: add.clone(), remove: remove.clone() });
    }

    /// Who holds `hash`? Servers: online holders as aggregated by the host (members' `Have`
    /// announcements and clip libraries). DMs: the contact. Never includes me.
    pub async fn who_has(&self, conv: &ConvId, hash: BlobHash) -> Vec<(UserId, AddrHint)> {
        if let Some(peer) = dm::peer_of(&self.inner, conv) {
            return vec![(peer, self.inner.hint(&peer).unwrap_or_default())];
        }
        let Some(tx) = self.inner.sessions.lock().get(conv).map(|s| s.tx.clone()) else { return Vec::new() };
        let (reply, rx) = oneshot::channel();
        if tx.send(SessionCmd::WhoHas { hash, reply }).await.is_err() {
            return Vec::new();
        }
        let me = self.inner.me;
        match tokio::time::timeout(self.inner.timings.who_has, rx).await {
            Ok(Ok(v)) => v.into_iter().filter(|(u, _)| *u != me).collect(),
            _ => Vec::new(),
        }
    }

    /// Tell others I am typing in `channel` of `conv` (server or DM).
    pub async fn typing(&self, conv: ConvId, channel: ChannelId) {
        if let Some(peer) = dm::peer_of(&self.inner, &conv) {
            let _ = self.inner.dm.send(&peer, &Ctl::Typing { channel: dm_channel(&conv), by: self.inner.me });
            return;
        }
        let tx = self.inner.sessions.lock().get(&conv).map(|s| s.tx.clone());
        if let Some(tx) = tx {
            let _ = tx.send(SessionCmd::Typing { channel }).await;
        }
    }

    /// Open (create if needed) the DM conversation with `user`; the contact name comes from
    /// a shared server. Also connects once in the background to sync any history.
    pub async fn dm_open(&self, user: UserId) -> Result<ConvId> {
        if user == self.inner.me {
            return Err(NetError::Invalid("You can't message yourself.".into()));
        }
        let (conv, _) = dm::open_conv(&self.inner, user)?;
        self.inner.dm.ensure_connect(&self.inner, user);
        Ok(conv)
    }

    /// DMs: my highest seq the contact has NOT confirmed (0 if everything is delivered).
    pub fn dm_pending(&self, conv: &ConvId) -> u64 {
        let Some(peer) = dm::peer_of(&self.inner, conv) else { return 0 };
        let head = self.inner.replica(conv).map(|r| r.lock().state.head(&self.inner.me)).unwrap_or(0);
        if head > self.inner.dm.delivered(&self.inner, conv, &peer) { head } else { 0 }
    }

    /// DMs: highest seq of mine the contact confirmed. My messages with a higher seq are
    /// pending.
    pub fn dm_delivered(&self, conv: &ConvId) -> u64 {
        match dm::peer_of(&self.inner, conv) {
            Some(peer) => self.inner.dm.delivered(&self.inner, conv, &peer),
            None => 0,
        }
    }

    /// Is `user` online in any roster I know of, or connected over a DM?
    pub fn peer_online(&self, user: &UserId) -> bool { self.inner.peer_online(user) }

    /// Clean shutdown: end the call and leave the voice room, hand off every server I host,
    /// close all connections and shut the router down (which closes the endpoint and the
    /// extra protocols).
    pub async fn shutdown(&self) {
        let inner = &self.inner;
        if inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        inner.media.shutdown(inner).await;
        let txs: Vec<mpsc::Sender<SessionCmd>> = inner.sessions.lock().values().map(|s| s.tx.clone()).collect();
        let mut waits = Vec::new();
        for tx in txs {
            let (reply, rx) = oneshot::channel();
            if tx.send(SessionCmd::Stop { handoff: true, reply }).await.is_ok() {
                waits.push(rx);
            }
        }
        let budget = inner.timings.handoff_ack + Duration::from_secs(2);
        let _ = tokio::time::timeout(budget, async {
            for w in waits {
                let _ = w.await;
            }
        })
        .await;
        inner.dm.close_all();
        inner.sessions.lock().clear();
        let router = inner.router.lock().take();
        if let Some(r) = router
            && let Err(e) = r.shutdown().await
        {
            tracing::warn!(error = %e, "router shutdown failed");
        }
        inner.tasks.abort_all();
        tracing::info!("network stopped");
    }

    /// Shared state (the media API lives in another module).
    pub(crate) fn inner(&self) -> &Arc<Inner> { &self.inner }

    // ------------------------------------------------------------------ part B hooks

    /// Register a tap for media, voice and call control messages (see [`crate::hooks`]). The
    /// media layer handles them first; the tap only observes.
    pub fn set_extra_ctl_handler(&self, handler: Arc<dyn ExtraCtlHandler>) { *self.inner.extra.write() = Some(handler); }

    /// Member role: the host and the connection to it.
    pub fn host_connection(&self, server: &ServerId) -> Option<(UserId, Connection)> {
        self.inner.sessions.lock().get(server).and_then(|s| s.shared.host_conn.lock().clone())
    }

    /// Host role: connected members with their short ids and connections.
    pub fn member_connections(&self, server: &ServerId) -> Vec<(UserId, u16, Connection)> {
        self.inner
            .sessions
            .lock()
            .get(server)
            .map(|s| s.shared.member_conns.lock().iter().map(|(u, (id, c))| (*u, *id, c.clone())).collect())
            .unwrap_or_default()
    }

    /// Member role: send a control message to the host. False if not connected.
    pub async fn send_ctl_to_host(&self, server: &ServerId, ctl: Ctl) -> bool { self.send_ctl(server, CtlTarget::Host, ctl).await }

    /// Host role: send a control message to one member. False if not connected.
    pub async fn send_ctl_to_member(&self, server: &ServerId, member: UserId, ctl: Ctl) -> bool {
        self.send_ctl(server, CtlTarget::Member(member), ctl).await
    }

    /// Host role: send a control message to every member (optionally except one).
    pub async fn broadcast_ctl(&self, server: &ServerId, ctl: Ctl, except: Option<UserId>) -> bool {
        self.send_ctl(server, CtlTarget::Members { except }, ctl).await
    }

    async fn send_ctl(&self, server: &ServerId, to: CtlTarget, ctl: Ctl) -> bool {
        let Some(tx) = self.inner.sessions.lock().get(server).map(|s| s.tx.clone()) else { return false };
        let (reply, rx) = oneshot::channel();
        if tx.send(SessionCmd::SendCtl { to, ctl, reply }).await.is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// My short id in `server` for the current host epoch (media track ids derive from it).
    pub fn my_short_id(&self, server: &ServerId) -> Option<u16> {
        self.inner.sessions.lock().get(server).and_then(|s| *s.shared.my_short_id.lock())
    }

    /// The DM connection to `peer`, if up.
    pub fn dm_connection(&self, peer: &UserId) -> Option<Connection> { self.inner.dm.connection(peer) }

    /// Connect to `peer` over `tincan/dm/1` now (opens the DM conversation if needed).
    pub async fn dm_connect(&self, peer: UserId) -> Result<Connection> {
        dm::open_conv(&self.inner, peer)?;
        self.inner.dm.connect(&self.inner, peer).await
    }

    /// Keep the DM connection to `peer` open while the guard lives (calls).
    pub fn dm_hold(&self, peer: UserId) -> DmHold { self.inner.dm.hold(&self.inner, peer) }

    /// Send a control message over the DM connection to `peer`. False if not connected.
    pub fn send_dm_ctl(&self, peer: &UserId, ctl: &Ctl) -> bool { self.inner.dm.send(peer, ctl) }

    fn notify_sessions(&self, mk: impl Fn() -> SessionCmd) {
        for s in self.inner.sessions.lock().values() {
            let _ = s.tx.try_send(mk());
        }
    }

    // ------------------------------------------------------------------ test hooks

    /// Test hook: park every session actor without closing anything (a hung or crashed
    /// process as seen by peers: they only notice through silence).
    #[cfg(test)]
    pub(crate) fn crash(&self) { self.inner.freeze.freeze(); }
}

/// A peer became reachable: poke sessions and DMs that may be waiting for it.
fn wake(inner: &Arc<Inner>, user: UserId) {
    for s in inner.sessions.lock().values() {
        let _ = s.tx.try_send(SessionCmd::PeerSeen { user });
    }
    inner.dm.peers_seen(inner, &[user]);
}

/// Dial bootstrap peers (following redirects) until a host accepts the join.
/// Returns the host, the connection (reply = JoinAccepted) and my profile event.
async fn join_dial(inner: &Arc<Inner>, invite: &Invite, name: &str, seq: u64, hlc: u64) -> Result<(UserId, Dialed, StoredEvent)> {
    let (tx, mut rx) = mpsc::channel::<(UserId, std::result::Result<Dialed, link::DialError>)>(16);
    let mut tried: HashSet<UserId> = HashSet::new();
    let mut outstanding = 0usize;
    let spawn_try = |user: UserId, delay: Duration, tried: &mut HashSet<UserId>, outstanding: &mut usize| -> Result<()> {
        if user == inner.me || !tried.insert(user) {
            return Ok(());
        }
        let req = build::join_request(&inner.identity, invite, &user, name, seq, hlc, now_ms()).map_err(NetError::from_core_user)?;
        let ep = inner.endpoint.clone();
        let hint = inner.hint(&user);
        let timeout = inner.timings.dial;
        let tx = tx.clone();
        *outstanding += 1;
        inner.tasks.spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let res = match link::connect(&ep, &user, hint.as_ref(), ALPN_SERVER, timeout).await {
                Ok(c) => link::exchange(c, &req, timeout).await,
                Err(e) => Err(e),
            };
            let _ = tx.send((user, res)).await;
        });
        Ok(())
    };
    for (i, (user, hint)) in invite.bootstrap.iter().enumerate() {
        inner.learn_hint(user, hint);
        spawn_try(*user, inner.timings.dial_stagger * u32::try_from(i).unwrap_or(0), &mut tried, &mut outstanding)?;
    }
    // In parallel: the rendezvous record names the current host and who is online.
    let (found_tx, mut found_rx) = mpsc::channel::<Option<tincan_core::rdv::RdvRecord>>(1);
    let mut resolving = false;
    if let (Some(rdv), Ok(rdv_pk)) = (inner.rdv.clone(), iroh::PublicKey::from_bytes(&invite.rdv_pk)) {
        resolving = true;
        let read_key = invite.rdv_read_key;
        inner.tasks.spawn(async move {
            let found = rdv.resolve_first(&rdv_pk, &read_key).await;
            let _ = found_tx.send(found).await;
        });
    }
    if outstanding == 0 && !resolving {
        return Err(NetError::Invalid("This invite has nobody to contact.".into()));
    }
    let mut best: Option<NetError> = None;
    while outstanding > 0 || resolving {
        let (user, res) = tokio::select! {
            found = found_rx.recv(), if resolving => {
                resolving = false;
                if let Some(Some(rec)) = found {
                    tracing::debug!(host = %rec.host.short(), epoch = rec.epoch, "join: rendezvous record found");
                    if let Some(relay) = &rec.host_relay {
                        inner.learn_hint(&rec.host, &AddrHint { relay: Some(relay.clone()), direct: Vec::new() });
                    }
                    for u in std::iter::once(rec.host).chain(rec.online.iter().copied()) {
                        spawn_try(u, Duration::ZERO, &mut tried, &mut outstanding)?;
                    }
                }
                continue;
            }
            got = rx.recv() => match got {
                Some(x) => x,
                None => break,
            },
        };
        outstanding -= 1;
        let d = match res {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!(peer = %user.short(), error = %e, "join dial failed");
                continue;
            }
        };
        match d.reply.clone() {
            Ctl::JoinAccepted { .. } => {
                let profile = match build::join_request(&inner.identity, invite, &user, name, seq, hlc, now_ms()) {
                    Ok(Ctl::JoinRequest { profile, .. }) => StoredEvent::from_signed(profile)?,
                    Ok(_) => return Err(NetError::Invalid("Could not build the join request.".into())),
                    Err(e) => return Err(NetError::from_core_user(e)),
                };
                return Ok((user, d, profile));
            }
            Ctl::Redirect { claim, host, addr } => {
                link::close_conn(&d.conn, tincan_core::CLOSE_NORMAL, "redirected");
                if claim.verify_for(&invite.server).is_ok() && claim.claim.host == host {
                    inner.learn_hint(&host, &addr);
                    spawn_try(host, Duration::ZERO, &mut tried, &mut outstanding)?;
                }
            }
            Ctl::Reject { code, reason } => {
                link::close_conn(&d.conn, tincan_core::CLOSE_NORMAL, "rejected");
                let err = match code {
                    RejectCode::InviteExpired => Some(NetError::Rejected("This invite has expired.".into())),
                    RejectCode::BadInvite => Some(NetError::Rejected(capitalize(&reason))),
                    RejectCode::Version => Some(NetError::Rejected("That server runs an incompatible Tincan version.".into())),
                    RejectCode::NotMember | RejectCode::NotHost | RejectCode::Internal => None,
                };
                if let Some(e) = err {
                    best = Some(e);
                }
            }
            _ => link::close_conn(&d.conn, tincan_core::CLOSE_NORMAL, "unexpected"),
        }
    }
    Err(best.unwrap_or_else(|| NetError::Unreachable(NO_ONE_REACHABLE.into())))
}

