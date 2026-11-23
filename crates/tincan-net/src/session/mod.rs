//! One actor task per server: cold open, member role, host role, election, migration and
//! partition merge (docs/architecture.md "Host election and migration").
//!
//! The actor owns the role state. Connections are [`Link`]s whose reader tasks feed
//! [`LinkEvent`]s into the actor; dials run as tasks that report a [`DialResult`]. Handlers
//! never await network I/O, so the actor stays responsive; role changes are returned as
//! [`Next`] values and applied by the main loop.

mod host;
mod member;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{Connection, SendStream};
use parking_lot::Mutex;
use tincan_core::build::{self, HelloInfo};
use tincan_core::election::SignedHostClaim;
use tincan_core::event::{Body, SignedEvent, StoredEvent};
use tincan_core::keys::ServerKeys;
use tincan_core::state::{Change, Verdict};
use tincan_core::storage::PeerRecord;
use tincan_core::sync::Heads;
use tincan_core::wire::{AddrHint, Ctl, RejectCode, Roster};
use tincan_core::{ALPN_SERVER, BlobHash, ChannelId, ServerId, UserId, now_ms};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::error::{NetError, Result};
use crate::events::{ConnState, ConnectionInfo, NetEvent, PathKind};
use crate::inner::Inner;
use crate::link::{self, DialError, Dialed, FrameReader, Link, LinkEvent};
use crate::replica::{ReplicaRef, send_missing};

pub(crate) use host::HostState;
pub(crate) use member::MemberState;

/// Commands from the `Net` handle (and the incoming-connection handler) to a session.
pub(crate) enum SessionCmd {
    /// A peer dialed us on `tincan/server/1` and sent `first` (Hello or JoinRequest).
    Incoming {
        /// The connection.
        conn: Connection,
        /// Control stream send half.
        send: SendStream,
        /// Control stream reader (after the first frame).
        reader: FrameReader,
        /// The first frame.
        first: Ctl,
    },
    /// Author and publish an event.
    Publish {
        /// The body (a `MemberLeave` gets `last_seq` filled in).
        body: Body,
        /// Result.
        reply: oneshot::Sender<Result<StoredEvent>>,
    },
    /// Who holds a blob?
    WhoHas {
        /// The blob.
        hash: BlobHash,
        /// Holders.
        reply: oneshot::Sender<Vec<(UserId, AddrHint)>>,
    },
    /// I am typing in a channel.
    Typing {
        /// The channel.
        channel: ChannelId,
    },
    /// My status changed.
    StatusChanged,
    /// My held blobs changed.
    HaveChanged {
        /// Added.
        add: Vec<BlobHash>,
        /// Removed.
        remove: Vec<BlobHash>,
    },
    /// A peer became reachable (address learned, LAN discovery, seen online elsewhere).
    PeerSeen {
        /// The peer.
        user: UserId,
    },
    /// Part B: send a control message.
    SendCtl {
        /// Where to.
        to: CtlTarget,
        /// The message.
        ctl: Ctl,
        /// True if it was queued on at least one link.
        reply: oneshot::Sender<bool>,
    },
    /// End the session (clean handoff first when hosting and `handoff`).
    Stop {
        /// Hand off to S1 if hosting.
        handoff: bool,
        /// Sent when done.
        reply: oneshot::Sender<()>,
    },
}

/// Destination of `SessionCmd::SendCtl`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CtlTarget {
    /// The host (member role).
    Host,
    /// One member (host role).
    Member(UserId),
    /// Every member (host role), optionally except one.
    Members {
        /// Skip this member.
        except: Option<UserId>,
    },
}

/// What other tasks can read about a session without asking the actor.
#[derive(Debug, Default)]
pub(crate) struct SessionShared {
    /// Current connection status.
    pub info: Mutex<ConnectionInfo>,
    /// Latest roster.
    pub roster: Mutex<Option<Roster>>,
    /// Member role: the connection to the host.
    pub host_conn: Mutex<Option<(UserId, Connection)>>,
    /// Host role: member connections with their short ids.
    pub member_conns: Mutex<HashMap<UserId, (u16, Connection)>>,
    /// My short id in the current epoch (media track ids derive from it).
    pub my_short_id: Mutex<Option<u16>>,
    /// Host role: the roster changed (the rendezvous publisher debounces on it).
    pub rdv_kick: tokio::sync::Notify,
}

/// Handle to a session actor.
#[derive(Clone, Debug)]
pub(crate) struct SessionHandle {
    /// Command channel.
    pub tx: mpsc::Sender<SessionCmd>,
    /// Shared view.
    pub shared: Arc<SessionShared>,
}

/// How a session starts.
pub(crate) enum StartMode {
    /// Cold open (app start).
    ColdOpen,
    /// A server just created: solo host at epoch 1.
    NewServer,
    /// Just joined: `dialed.reply` is the host's Welcome.
    Joined(Box<Dialed>),
}

/// Why a dial was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DialPurpose {
    /// Looking for the host.
    Seek,
    /// Hosting: looking for another host of the same server (partition merge).
    Probe,
}

/// Outcome of a dial task.
pub(super) struct DialResult {
    user: UserId,
    purpose: DialPurpose,
    result: std::result::Result<Dialed, DialError>,
}

/// A queued (unsigned) publish.
struct Queued {
    body: Body,
    reply: oneshot::Sender<Result<StoredEvent>>,
    deadline: Instant,
}

/// A signed publish waiting for the host's ack.
pub(super) struct Inflight {
    event: StoredEvent,
    reply: Option<oneshot::Sender<Result<StoredEvent>>>,
    deadline: Instant,
    sent_on: Option<u64>,
    gap_retries: u32,
    retry_at: Option<Instant>,
}

/// Publish pipeline (one event in flight at a time, so seqs stay contiguous at the host).
#[derive(Default)]
pub(super) struct Publishes {
    queued: VecDeque<Queued>,
    inflight: Option<Inflight>,
}

/// Seeking a host.
pub(super) struct Seek {
    kind: SeekKind,
    /// Promotion (failover), solo (cold open) or fallback deadline.
    deadline: Instant,
    /// Last time a higher-ranked successor answered without being host yet.
    alive_above: Option<Instant>,
}

/// Why we are seeking.
#[derive(Clone, Debug)]
pub(super) enum SeekKind {
    /// App start or nothing else worked.
    ColdOpen,
    /// The host went away (or moved).
    Failover {
        /// The lost host.
        failed_host: UserId,
        /// Its epoch.
        failed_epoch: u64,
        /// Successors in order (the failed host excluded).
        succession: Vec<UserId>,
        /// My rank in `succession`, if listed.
        rank: Option<usize>,
    },
}

/// The role state.
pub(super) enum Role {
    /// Looking for a host.
    Seeking(Seek),
    /// Connected to a host.
    Member(Box<MemberState>),
    /// Hosting.
    Host(Box<HostState>),
    /// Not allowed to host and nobody reachable; retry later.
    Offline {
        /// Next cold open.
        retry_at: Instant,
    },
    /// The actor is ending.
    Stopped,
}

/// How to become a member.
pub(super) struct MemberStart {
    /// The connection whose reply was the Welcome.
    pub link: Box<Dialed>,
    /// The host's claim.
    pub claim: SignedHostClaim,
    /// Roster from Welcome.
    pub roster: Roster,
    /// My short id.
    pub short_id: u16,
    /// Host's view of my next seq.
    pub next_seq: u64,
}

/// How to become host.
pub(super) struct HostStart {
    /// Epoch to claim.
    pub epoch: u64,
    /// Previous host.
    pub prev: Option<UserId>,
    /// Roster to inherit short ids from.
    pub inherit: Option<Roster>,
    /// A member to accept right away (its link and Hello).
    pub accept: Option<(Link, Ctl)>,
    /// Links to keep only until the peer closes them (old host after a handoff).
    pub retire: Vec<Link>,
    /// Reply to send on the first retired link after claiming (handoff: HostMoved + Ack).
    pub handoff_reply: bool,
}

/// A role change requested by a handler.
pub(super) enum Next {
    /// Nothing.
    Stay,
    /// Become a member.
    Member(Box<MemberStart>),
    /// Become host.
    Host(Box<HostStart>),
    /// Start seeking; dial `first` before anything else.
    Seek(SeekKind, Option<UserId>),
    /// Give up for now (cannot host).
    Offline,
    /// End the actor.
    Stop,
}

/// Everything but the role.
pub(super) struct Ctx {
    pub net: Arc<Inner>,
    pub server: ServerId,
    pub me: UserId,
    pub replica: ReplicaRef,
    pub shared: Arc<SessionShared>,
    pub link_tx: mpsc::Sender<LinkEvent>,
    dial_tx: mpsc::Sender<DialResult>,
    next_id: u64,
    /// Highest host epoch seen.
    pub epoch_seen: u64,
    /// Last roster seen or built (succession, short ids, candidates).
    pub last_roster: Option<Roster>,
    pub pubs: Publishes,
    pub whohas: HashMap<BlobHash, Vec<HoldersReply>>,
    dialing: HashSet<UserId>,
    redials: Vec<Redial>,
    /// Next liveness tick (only while links exist).
    pub tick_at: Option<Instant>,
    pub ticks: u64,
    pub last_status_at: Instant,
    pub last_ac: bool,
    persisted: HashMap<UserId, (AddrHint, u64)>,
    roster_sig: Option<(Vec<UserId>, Vec<UserId>)>,
}

/// Reply channel of a `who_has` request.
type HoldersReply = oneshot::Sender<Vec<(UserId, AddrHint)>>;

/// A scheduled redial: when, whom, why.
type Redial = (Instant, UserId, DialPurpose);

/// Peers dialed per cold-open round.
const COLD_OPEN_PEERS: usize = 6;
/// Re-persist a peer's last-seen time at most this often.
const PEER_PERSIST_EVERY_MS: u64 = 5 * 60 * 1000;
/// Publish retries when the host reports a gap.
const MAX_GAP_RETRIES: u32 = 5;
/// Delay before re-sending a publish the host reported as a gap.
const GAP_RETRY: Duration = Duration::from_millis(500);
/// Command queue length.
const CMD_QUEUE: usize = 256;
/// Link event queue length.
const LINK_QUEUE: usize = 1024;

fn roster_key(server: &ServerId) -> String { format!("net/roster/{}", server.to_hex()) }

/// Spawn the session actor of `server`.
pub(crate) fn spawn(net: &Arc<Inner>, server: ServerId, replica: ReplicaRef, mode: StartMode) -> SessionHandle {
    let (tx, cmd_rx) = mpsc::channel(CMD_QUEUE);
    let (link_tx, link_rx) = mpsc::channel(LINK_QUEUE);
    let (dial_tx, dial_rx) = mpsc::channel(64);
    let shared = Arc::new(SessionShared::default());
    let last_roster = match net.storage.get_kv(&roster_key(&server)) {
        Ok(Some(b)) => postcard::from_bytes::<Roster>(&b).ok(),
        _ => None,
    };
    let epoch_seen = replica.lock().record.last_epoch;
    // Visible right away, before the actor's first step.
    *shared.info.lock() = match &mode {
        StartMode::NewServer => {
            ConnectionInfo { state: ConnState::Solo, host: Some(net.me), path: PathKind::None, rtt_ms: None, epoch: epoch_seen + 1 }
        }
        StartMode::Joined(d) => ConnectionInfo {
            state: ConnState::Member,
            host: Some(crate::addr::remote_user(&d.conn)),
            path: PathKind::None,
            rtt_ms: None,
            epoch: epoch_seen,
        },
        StartMode::ColdOpen => ConnectionInfo { epoch: epoch_seen, ..ConnectionInfo::default() },
    };
    let ctx = Ctx {
        net: net.clone(),
        server,
        me: net.me,
        replica,
        shared: shared.clone(),
        link_tx,
        dial_tx,
        next_id: 1,
        epoch_seen,
        last_roster,
        pubs: Publishes::default(),
        whohas: HashMap::new(),
        dialing: HashSet::new(),
        redials: Vec::new(),
        tick_at: None,
        ticks: 0,
        last_status_at: Instant::now(),
        last_ac: crate::power::ac_power(),
        persisted: HashMap::new(),
        roster_sig: None,
    };
    let session = Session { ctx, role: Role::Offline { retry_at: Instant::now() } };
    net.tasks.spawn(session.run(mode, cmd_rx, link_rx, dial_rx));
    SessionHandle { tx, shared }
}

/// The actor.
pub(super) struct Session {
    ctx: Ctx,
    role: Role,
}

async fn sleep_opt(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending::<()>().await,
    }
}

impl Session {
    async fn run(
        mut self,
        mode: StartMode,
        mut cmd_rx: mpsc::Receiver<SessionCmd>,
        mut link_rx: mpsc::Receiver<LinkEvent>,
        mut dial_rx: mpsc::Receiver<DialResult>,
    ) {
        let first = match mode {
            StartMode::ColdOpen => Next::Seek(SeekKind::ColdOpen, None),
            StartMode::NewServer => Next::Host(Box::new(HostStart {
                epoch: self.ctx.epoch_seen + 1,
                prev: None,
                inherit: None,
                accept: None,
                retire: Vec::new(),
                handoff_reply: false,
            })),
            StartMode::Joined(dialed) => match self.ctx.welcome_start(dialed) {
                Some(start) => Next::Member(Box::new(start)),
                None => Next::Seek(SeekKind::ColdOpen, None),
            },
        };
        if self.apply(first) {
            return;
        }
        loop {
            let wake = self.next_wake();
            let next = tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(c) => self.on_cmd(c),
                    None => Next::Stop,
                },
                Some(ev) = link_rx.recv() => self.on_link(ev),
                Some(d) = dial_rx.recv() => self.on_dial(d),
                _ = sleep_opt(wake) => self.on_timer(),
                _ = self.ctx.net.freeze.wait_frozen() => {
                    // Hung: hold every connection open and do nothing, forever.
                    std::future::pending::<()>().await;
                    Next::Stop
                }
            };
            if self.apply(next) {
                break;
            }
        }
        self.finish();
    }

    /// Apply a role change. True if the actor must stop.
    fn apply(&mut self, next: Next) -> bool {
        match next {
            Next::Stay => false,
            Next::Stop => {
                self.stop_now();
                true
            }
            Next::Member(start) => {
                let next = self.become_member(*start);
                self.apply(next)
            }
            Next::Host(start) => {
                let next = self.become_host(*start);
                self.apply(next)
            }
            Next::Seek(kind, first) => {
                let next = self.start_seek(kind, first);
                self.apply(next)
            }
            Next::Offline => {
                self.leave_role(tincan_core::CLOSE_NORMAL);
                self.role = Role::Offline { retry_at: Instant::now() + self.ctx.net.timings.solo_retry };
                self.ctx.update_info(&self.role);
                false
            }
        }
    }

    /// Close whatever the current role holds (used before switching).
    fn leave_role(&mut self, code: u32) {
        match std::mem::replace(&mut self.role, Role::Stopped) {
            Role::Member(m) => {
                m.link.close(code, "leaving");
                *self.ctx.shared.host_conn.lock() = None;
            }
            Role::Host(h) => {
                let links: Vec<Link> = h.into_links();
                for l in links {
                    l.close(code, "leaving");
                }
                self.ctx.shared.member_conns.lock().clear();
            }
            _ => {}
        }
        self.ctx.tick_at = None;
    }

    fn stop_now(&mut self) {
        self.leave_role(tincan_core::CLOSE_NORMAL);
        self.role = Role::Stopped;
    }

    fn finish(&mut self) {
        let mut info = self.ctx.shared.info.lock().clone();
        info.state = ConnState::Offline;
        info.path = PathKind::None;
        info.rtt_ms = None;
        *self.ctx.shared.info.lock() = info;
        *self.ctx.shared.roster.lock() = None;
        for q in self.ctx.pubs.queued.drain(..) {
            let _ = q.reply.send(Err(NetError::Closed));
        }
        if let Some(i) = self.ctx.pubs.inflight.take()
            && let Some(r) = i.reply
        {
            let _ = r.send(Err(NetError::Closed));
        }
    }

    fn next_wake(&self) -> Option<Instant> {
        let mut wake: Option<Instant> = self.ctx.tick_at;
        let mut add = |t: Option<Instant>| {
            if let Some(t) = t {
                wake = Some(wake.map_or(t, |w| w.min(t)));
            }
        };
        for (t, _, _) in &self.ctx.redials {
            add(Some(*t));
        }
        for q in &self.ctx.pubs.queued {
            add(Some(q.deadline));
        }
        if let Some(i) = &self.ctx.pubs.inflight {
            if i.reply.is_some() {
                add(Some(i.deadline));
            }
            add(i.retry_at);
        }
        match &self.role {
            Role::Seeking(s) => add(Some(s.deadline)),
            Role::Offline { retry_at } => add(Some(*retry_at)),
            Role::Host(h) => {
                for t in h.wakeups() {
                    add(Some(t));
                }
            }
            Role::Member(_) | Role::Stopped => {}
        }
        wake
    }

    fn on_cmd(&mut self, cmd: SessionCmd) -> Next {
        match cmd {
            SessionCmd::Incoming { conn, send, reader, first } => self.on_incoming(conn, send, reader, first),
            SessionCmd::Publish { body, reply } => {
                let deadline = Instant::now() + self.ctx.net.timings.publish_timeout;
                self.ctx.pubs.queued.push_back(Queued { body, reply, deadline });
                self.pump_publishes();
                Next::Stay
            }
            SessionCmd::WhoHas { hash, reply } => {
                match &mut self.role {
                    Role::Host(h) => {
                        let holders = h.holders(&self.ctx, &hash, None);
                        let _ = reply.send(holders);
                    }
                    Role::Member(m) if m.link.send(&Ctl::WhoHas { hash }) => {
                        self.ctx.whohas.entry(hash).or_default().push(reply);
                    }
                    _ => {
                        let _ = reply.send(Vec::new());
                    }
                }
                Next::Stay
            }
            SessionCmd::Typing { channel } => {
                let ctl = Ctl::Typing { channel, by: self.ctx.me };
                match &mut self.role {
                    Role::Host(h) => h.broadcast(&self.ctx, &ctl, None),
                    Role::Member(m) => {
                        let _ = m.link.send(&ctl);
                    }
                    _ => {}
                }
                Next::Stay
            }
            SessionCmd::StatusChanged => {
                match &mut self.role {
                    Role::Host(h) => h.mark_roster(&self.ctx),
                    Role::Member(m) => {
                        let status = self.ctx.net.status_for(&self.ctx.server, m.synced);
                        let _ = m.link.send(&Ctl::Status(status));
                        self.ctx.last_status_at = Instant::now();
                    }
                    _ => {}
                }
                Next::Stay
            }
            SessionCmd::HaveChanged { add, remove } => {
                if let Role::Member(m) = &mut self.role {
                    let _ = m.link.send(&Ctl::Have { add, remove });
                }
                Next::Stay
            }
            SessionCmd::PeerSeen { user } => self.on_peer_seen(user),
            SessionCmd::SendCtl { to, ctl, reply } => {
                let ok = match (&mut self.role, to) {
                    (Role::Member(m), CtlTarget::Host) => m.link.send(&ctl),
                    (Role::Host(h), CtlTarget::Member(u)) => h.links.get(&u).is_some_and(|l| l.link.send(&ctl)),
                    (Role::Host(h), CtlTarget::Members { except }) => {
                        h.broadcast(&self.ctx, &ctl, except);
                        true
                    }
                    _ => false,
                };
                let _ = reply.send(ok);
                Next::Stay
            }
            SessionCmd::Stop { handoff, reply } => match &mut self.role {
                Role::Host(h) if handoff => h.start_handoff(&mut self.ctx, host::AfterHandoff::Stop(reply)),
                _ => {
                    let _ = reply.send(());
                    Next::Stop
                }
            },
        }
    }

    fn on_peer_seen(&mut self, user: UserId) -> Next {
        if user == self.ctx.me || !self.ctx.is_member(&user) {
            return Next::Stay;
        }
        match &self.role {
            Role::Seeking(_) => self.ctx.dial(user, DialPurpose::Seek, Duration::ZERO),
            Role::Host(h) if !h.links.contains_key(&user) => self.ctx.dial(user, DialPurpose::Probe, Duration::ZERO),
            Role::Offline { .. } => return Next::Seek(SeekKind::ColdOpen, Some(user)),
            _ => {}
        }
        Next::Stay
    }

    fn on_link(&mut self, ev: LinkEvent) -> Next {
        match &mut self.role {
            Role::Member(_) => member::on_link(self, ev),
            Role::Host(_) => host::on_link(self, ev),
            _ => Next::Stay,
        }
    }

    fn on_dial(&mut self, d: DialResult) -> Next {
        self.ctx.dialing.remove(&d.user);
        let dialed = match d.result {
            Ok(dialed) => dialed,
            Err(e) => {
                tracing::debug!(server = %self.ctx.server.short(), peer = %d.user.short(), error = %e, "dial failed");
                return self.on_dial_failed(d.user);
            }
        };
        match dialed.reply.clone() {
            Ctl::Welcome { claim, roster, your_short_id, your_next_seq } => {
                let valid = claim.verify_for(&self.ctx.server).is_ok() && claim.claim.host == d.user && roster.host == d.user;
                if !valid {
                    link::close_conn(&dialed.conn, tincan_core::CLOSE_NORMAL, "bad welcome");
                    return Next::Stay;
                }
                self.ctx.epoch_seen = self.ctx.epoch_seen.max(claim.claim.epoch);
                let start = MemberStart {
                    link: Box::new(dialed),
                    claim,
                    roster,
                    short_id: your_short_id,
                    next_seq: your_next_seq,
                };
                match &mut self.role {
                    Role::Seeking(_) | Role::Offline { .. } => Next::Member(Box::new(start)),
                    Role::Host(_) => host::start_probe_merge(self, d.user, start),
                    _ => {
                        link::close_conn(&start.link.conn, tincan_core::CLOSE_NORMAL, "already connected");
                        Next::Stay
                    }
                }
            }
            Ctl::Redirect { claim, host, addr } => {
                link::close_conn(&dialed.conn, tincan_core::CLOSE_NORMAL, "redirected");
                self.on_redirect(d.user, d.purpose, claim, host, addr)
            }
            Ctl::Reject { code, reason } => {
                link::close_conn(&dialed.conn, tincan_core::CLOSE_NORMAL, "rejected");
                tracing::debug!(server = %self.ctx.server.short(), peer = %d.user.short(), ?code, %reason, "dial rejected");
                if code == RejectCode::NotHost {
                    self.note_alive_not_host(d.user);
                }
                Next::Stay
            }
            other => {
                tracing::debug!(?other, "unexpected first reply");
                link::close_conn(&dialed.conn, tincan_core::CLOSE_NORMAL, "unexpected reply");
                Next::Stay
            }
        }
    }

    /// A peer answered but is not hosting.
    fn note_alive_not_host(&mut self, user: UserId) {
        if let Role::Seeking(s) = &mut self.role
            && let SeekKind::Failover { succession, rank, .. } = &s.kind
            && let Some(pos) = succession.iter().position(|u| *u == user)
        {
            if rank.is_none_or(|r| pos < r) {
                s.alive_above = Some(Instant::now());
            }
            let at = Instant::now() + self.ctx.net.timings.failover_redial;
            self.ctx.redials.push((at, user, DialPurpose::Seek));
        }
    }

    fn on_dial_failed(&mut self, user: UserId) -> Next {
        if let Role::Seeking(s) = &self.role
            && let SeekKind::Failover { succession, .. } = &s.kind
            && succession.contains(&user)
        {
            let at = Instant::now() + self.ctx.net.timings.failover_redial;
            self.ctx.redials.push((at, user, DialPurpose::Seek));
        }
        Next::Stay
    }

    fn on_redirect(&mut self, from: UserId, purpose: DialPurpose, claim: SignedHostClaim, host: UserId, addr: AddrHint) -> Next {
        if claim.verify_for(&self.ctx.server).is_err() || claim.claim.host != host {
            return Next::Stay;
        }
        if host == self.ctx.me {
            return Next::Stay;
        }
        self.ctx.net.learn_hint(&host, &addr);
        match &self.role {
            Role::Seeking(s) => {
                if let SeekKind::Failover { failed_host, failed_epoch, .. } = &s.kind
                    && host == *failed_host
                    && claim.claim.epoch <= *failed_epoch
                {
                    // The redirecting peer has not noticed the host is gone yet.
                    self.note_alive_not_host(from);
                    self.ctx.dial(host, DialPurpose::Seek, Duration::ZERO);
                    return Next::Stay;
                }
                self.ctx.dial(host, DialPurpose::Seek, Duration::ZERO);
            }
            Role::Offline { .. } => self.ctx.dial(host, DialPurpose::Seek, Duration::ZERO),
            Role::Host(h) if h.claim.claim.host != host && h.merge.is_none() => {
                let _ = purpose;
                self.ctx.dial(host, DialPurpose::Probe, Duration::ZERO);
            }
            _ => {}
        }
        Next::Stay
    }

    fn on_incoming(&mut self, conn: Connection, send: SendStream, reader: FrameReader, first: Ctl) -> Next {
        let peer = crate::addr::remote_user(&conn);
        let wait = Duration::from_secs(1);
        let reject = |ctx: &Ctx, conn: Connection, send: SendStream, code: RejectCode, reason: &str| {
            let ctl = Ctl::Reject { code, reason: reason.to_string() };
            ctx.net.tasks.spawn(async move { link::reply_and_close(conn, send, &ctl, tincan_core::CLOSE_REJECTED, wait).await });
        };
        match &first {
            Ctl::Hello { .. } => {
                let (mac_ok, member) = self.ctx.check_hello(&peer, &first);
                if !mac_ok {
                    reject(&self.ctx, conn, send, RejectCode::NotMember, "not a member");
                    return Next::Stay;
                }
                match &mut self.role {
                    Role::Host(h) => {
                        if !member {
                            reject(&self.ctx, conn, send, RejectCode::NotMember, "not a member");
                            return Next::Stay;
                        }
                        if h.handoff.is_some() {
                            reject(&self.ctx, conn, send, RejectCode::NotHost, "host is moving");
                            return Next::Stay;
                        }
                        let link = self.ctx.spawn_link(conn, send, reader);
                        h.accept_member(&mut self.ctx, link, first);
                        Next::Stay
                    }
                    Role::Member(m) => {
                        let ctl = Ctl::Redirect {
                            claim: m.claim.clone(),
                            host: m.host,
                            addr: self.ctx.host_addr(&m.host, &m.roster),
                        };
                        self.ctx.net.tasks.spawn(async move {
                            link::reply_and_close(conn, send, &ctl, tincan_core::CLOSE_REDIRECT, wait).await
                        });
                        Next::Stay
                    }
                    Role::Seeking(Seek { kind: SeekKind::ColdOpen, .. }) | Role::Offline { .. } => {
                        let their_epoch = match &first {
                            Ctl::Hello { epoch_seen, .. } => *epoch_seen,
                            _ => 0,
                        };
                        if member && self.ctx.net.allow_hosting() && self.ctx.me < peer {
                            // Two members looking for each other: the lower id hosts. Not while
                            // my dial to the last known host is still out (it may be alive).
                            let last_host = self.ctx.replica.lock().record.last_host;
                            if last_host.is_some_and(|h| h != self.ctx.me && self.ctx.dialing.contains(&h)) {
                                reject(&self.ctx, conn, send, RejectCode::NotHost, "still looking for the host");
                                if !self.ctx.redials.iter().any(|(_, u, _)| *u == peer) {
                                    let at = Instant::now() + self.ctx.net.timings.dial;
                                    self.ctx.redials.push((at, peer, DialPurpose::Seek));
                                }
                                return Next::Stay;
                            }
                            let link = self.ctx.spawn_link(conn, send, reader);
                            let epoch = self.ctx.epoch_seen.max(their_epoch) + 1;
                            let prev = self.ctx.replica.lock().record.last_host;
                            let inherit = self.ctx.last_roster.clone();
                            return Next::Host(Box::new(HostStart {
                                epoch,
                                prev,
                                inherit,
                                accept: Some((link, first)),
                                retire: Vec::new(),
                                handoff_reply: false,
                            }));
                        }
                        reject(&self.ctx, conn, send, RejectCode::NotHost, "not hosting");
                        if member {
                            self.ctx.dial(peer, DialPurpose::Seek, Duration::ZERO);
                        }
                        Next::Stay
                    }
                    Role::Seeking(_) | Role::Stopped => {
                        reject(&self.ctx, conn, send, RejectCode::NotHost, "not hosting");
                        Next::Stay
                    }
                }
            }
            Ctl::JoinRequest { .. } => match &mut self.role {
                Role::Host(h) if h.handoff.is_none() => {
                    match h.admit_joiner(&mut self.ctx, &peer, &first) {
                        Ok(accepted) => {
                            let link = self.ctx.spawn_link(conn, send, reader);
                            let _ = link.send(&accepted);
                            h.joiners.insert(link.id, link);
                        }
                        Err(rej) => {
                            let ctl = rej.to_ctl();
                            self.ctx.net.tasks.spawn(async move {
                                link::reply_and_close(conn, send, &ctl, tincan_core::CLOSE_REJECTED, wait).await
                            });
                        }
                    }
                    Next::Stay
                }
                Role::Member(m) => {
                    let ctl = Ctl::Redirect { claim: m.claim.clone(), host: m.host, addr: self.ctx.host_addr(&m.host, &m.roster) };
                    self.ctx.net.tasks.spawn(async move {
                        link::reply_and_close(conn, send, &ctl, tincan_core::CLOSE_REDIRECT, wait).await
                    });
                    Next::Stay
                }
                _ => {
                    reject(&self.ctx, conn, send, RejectCode::NotHost, "not hosting right now");
                    Next::Stay
                }
            },
            _ => {
                reject(&self.ctx, conn, send, RejectCode::Internal, "unexpected first frame");
                Next::Stay
            }
        }
    }

    fn on_timer(&mut self) -> Next {
        let now = Instant::now();
        // Scheduled redials.
        if !self.ctx.redials.is_empty() {
            let due: Vec<Redial> = {
                let (due, keep): (Vec<_>, Vec<_>) = self.ctx.redials.drain(..).partition(|(t, _, _)| *t <= now);
                self.ctx.redials = keep;
                due
            };
            let seeking = matches!(self.role, Role::Seeking(_));
            for (_, user, purpose) in due {
                if seeking || purpose == DialPurpose::Probe {
                    self.ctx.dial(user, purpose, Duration::ZERO);
                }
            }
        }
        self.publish_timers(now);
        let next = match &mut self.role {
            Role::Seeking(_) => self.seek_timer(now),
            Role::Offline { retry_at } if *retry_at <= now => Next::Seek(SeekKind::ColdOpen, None),
            Role::Member(_) => member::on_timer(self, now),
            Role::Host(_) => host::on_timer(self, now),
            _ => Next::Stay,
        };
        if matches!(next, Next::Stay) {
            self.pump_publishes();
        }
        next
    }

    fn seek_timer(&mut self, now: Instant) -> Next {
        let Role::Seeking(s) = &mut self.role else { return Next::Stay };
        if s.deadline > now {
            return Next::Stay;
        }
        let allow = self.ctx.net.allow_hosting();
        match &s.kind {
            SeekKind::ColdOpen => {
                if allow {
                    let epoch = self.ctx.epoch_seen + 1;
                    let prev = self.ctx.replica.lock().record.last_host;
                    tracing::info!(server = %self.ctx.server.short(), epoch, "nobody reachable: hosting solo");
                    Next::Host(Box::new(HostStart {
                        epoch,
                        prev,
                        inherit: self.ctx.last_roster.clone(),
                        accept: None,
                        retire: Vec::new(),
                        handoff_reply: false,
                    }))
                } else {
                    Next::Offline
                }
            }
            SeekKind::Failover { failed_host, rank, .. } => {
                let stagger = self.ctx.net.timings.stagger;
                match rank {
                    Some(_) if allow => {
                        if s.alive_above.is_some_and(|t| now.duration_since(t) < stagger) {
                            s.deadline = now + stagger;
                            return Next::Stay;
                        }
                        let epoch = self.ctx.epoch_seen + 1;
                        let prev = Some(*failed_host);
                        tracing::info!(server = %self.ctx.server.short(), epoch, "host lost: taking over as successor");
                        Next::Host(Box::new(HostStart {
                            epoch,
                            prev,
                            inherit: self.ctx.last_roster.clone(),
                            accept: None,
                            retire: Vec::new(),
                            handoff_reply: false,
                        }))
                    }
                    _ => {
                        if s.alive_above.is_some_and(|t| now.duration_since(t) < stagger) {
                            s.deadline = now + stagger;
                            return Next::Stay;
                        }
                        Next::Seek(SeekKind::ColdOpen, None)
                    }
                }
            }
        }
    }

    fn start_seek(&mut self, kind: SeekKind, first: Option<UserId>) -> Next {
        if !matches!(self.role, Role::Seeking(_)) {
            self.leave_role(tincan_core::CLOSE_NORMAL);
        }
        let t = self.ctx.net.timings.clone();
        let now = Instant::now();
        let mut order: Vec<UserId> = Vec::new();
        if let Some(f) = first {
            order.push(f);
        }
        let deadline = match &kind {
            SeekKind::ColdOpen => {
                order.extend(self.ctx.cold_candidates());
                // In parallel: the rendezvous record (relay and DHT).
                if let Some(keys) = self.ctx.keys() {
                    crate::rdv::spawn_cold_resolve(&self.ctx.net, self.ctx.server, keys);
                }
                now + t.solo_after
            }
            SeekKind::Failover { succession, rank, failed_host, .. } => {
                let above: Vec<UserId> = match rank {
                    Some(r) => succession[..*r].to_vec(),
                    None => succession.clone(),
                };
                order.extend(above);
                order.push(*failed_host);
                match rank {
                    Some(r) => now + t.stagger * u32::try_from(*r).unwrap_or(u32::MAX),
                    None => now + t.stagger * u32::try_from(succession.len()).unwrap_or(u32::MAX) + t.dial,
                }
            }
        };
        let mut seen = HashSet::new();
        order.retain(|u| *u != self.ctx.me && seen.insert(*u));
        let stagger = match kind {
            SeekKind::ColdOpen => t.dial_stagger,
            SeekKind::Failover { .. } => Duration::ZERO,
        };
        for (i, u) in order.into_iter().enumerate() {
            self.ctx.dial(u, DialPurpose::Seek, stagger * u32::try_from(i).unwrap_or(u32::MAX));
        }
        self.role = Role::Seeking(Seek { kind, deadline, alive_above: None });
        self.ctx.update_info(&self.role);
        // A rank-0 successor promotes at once.
        if deadline <= now { self.seek_timer(now) } else { Next::Stay }
    }

    fn become_member(&mut self, start: MemberStart) -> Next {
        self.leave_role(tincan_core::CLOSE_NORMAL);
        let d = *start.link;
        let link = self.ctx.spawn_link(d.conn, d.send, d.reader);
        let host = start.claim.claim.host;
        let epoch = start.claim.claim.epoch;
        self.ctx.epoch_seen = self.ctx.epoch_seen.max(epoch);
        self.ctx.update_record(epoch, host);
        self.ctx.redials.clear();
        let heads = self.ctx.heads();
        let _ = link.send(&Ctl::SyncHave { heads });
        *self.ctx.shared.host_conn.lock() = Some((host, link.conn.clone()));
        let path = crate::addr::path_snapshot(&link.conn);
        tracing::info!(server = %self.ctx.server.short(), host = %host.short(), epoch, "connected to host");
        *self.ctx.shared.my_short_id.lock() = Some(start.short_id);
        let media = self.ctx.net.media.member_started(&self.ctx.net, self.ctx.server, host, &link.conn, link.live(), epoch, start.short_id);
        let state = MemberState::new(link, start.claim, start.roster.clone(), start.next_seq, path, media);
        self.role = Role::Member(Box::new(state));
        self.ctx.note_roster(&start.roster);
        self.ctx.tick_at = Some(Instant::now() + self.ctx.net.timings.tick);
        self.ctx.update_info(&self.role);
        // Unacked publish from before: resend on the new link (after our sync went out).
        if let Some(i) = &mut self.ctx.pubs.inflight {
            i.sent_on = None;
        }
        Next::Stay
    }

    fn become_host(&mut self, start: HostStart) -> Next {
        self.leave_role(tincan_core::CLOSE_NORMAL);
        let claim = match build::host_claim(&self.ctx.net.identity, &self.ctx.server, start.epoch, start.prev, now_ms()) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "cannot sign host claim");
                return Next::Offline;
            }
        };
        self.ctx.epoch_seen = self.ctx.epoch_seen.max(start.epoch);
        self.ctx.update_record(start.epoch, self.ctx.me);
        self.ctx.redials.clear();
        let mut short_ids: HashMap<UserId, u16> = HashMap::new();
        for r in start.inherit.iter().chain(self.ctx.last_roster.iter()) {
            for e in &r.online {
                if !short_ids.contains_key(&e.user) && !short_ids.values().any(|v| *v == e.short_id) {
                    short_ids.insert(e.user, e.short_id);
                }
            }
        }
        let mut h = HostState::new(&self.ctx, claim, short_ids);
        *self.ctx.shared.my_short_id.lock() = h.short_ids.get(&self.ctx.me).copied();
        let mut retire = start.retire;
        if start.handoff_reply
            && let Some(old) = retire.first()
        {
            let moved = Ctl::HostMoved { claim: h.claim.clone(), addr: self.ctx.net.my_hint() };
            let _ = old.send(&moved);
            let _ = old.send(&Ctl::HandoffAck { epoch: start.epoch });
        }
        h.retiring.append(&mut retire);
        tracing::info!(server = %self.ctx.server.short(), epoch = start.epoch, "hosting");
        if let Some((link, hello)) = start.accept {
            h.accept_member(&mut self.ctx, link, hello);
        }
        h.mark_roster(&self.ctx);
        self.role = Role::Host(Box::new(h));
        self.ctx.update_info(&self.role);
        self.pump_publishes();
        Next::Stay
    }

    /// Publish timeouts (queued items fail; a signed in-flight event keeps retrying).
    fn publish_timers(&mut self, now: Instant) {
        let mut i = 0;
        while i < self.ctx.pubs.queued.len() {
            if self.ctx.pubs.queued[i].deadline <= now {
                if let Some(q) = self.ctx.pubs.queued.remove(i) {
                    let _ = q.reply.send(Err(NetError::NotConnected));
                }
            } else {
                i += 1;
            }
        }
        if let Some(inf) = &mut self.ctx.pubs.inflight {
            if inf.deadline <= now
                && let Some(r) = inf.reply.take()
            {
                let _ = r.send(Err(NetError::Timeout));
            }
            if inf.retry_at.is_some_and(|t| t <= now) {
                inf.retry_at = None;
                inf.sent_on = None;
            }
        }
    }

    /// Move the publish pipeline forward for the current role.
    fn pump_publishes(&mut self) {
        match &mut self.role {
            Role::Host(h) => {
                if let Some(inf) = self.ctx.pubs.inflight.take() {
                    let res = h.publish_local(&mut self.ctx, inf.event);
                    if let Some(r) = inf.reply {
                        let _ = r.send(res);
                    }
                }
                while let Some(q) = self.ctx.pubs.queued.pop_front() {
                    let res = self.ctx.sign_own(q.body, 0).and_then(|ev| h.publish_local(&mut self.ctx, ev));
                    let _ = q.reply.send(res);
                }
            }
            Role::Member(m) => {
                if !m.sync_out_done {
                    return;
                }
                if self.ctx.pubs.inflight.is_none()
                    && let Some(q) = self.ctx.pubs.queued.pop_front()
                {
                    match self.ctx.sign_own(q.body, m.next_seq_hint) {
                        Ok(ev) => {
                            self.ctx.pubs.inflight = Some(Inflight {
                                event: ev,
                                reply: Some(q.reply),
                                deadline: q.deadline,
                                sent_on: None,
                                gap_retries: 0,
                                retry_at: None,
                            });
                        }
                        Err(e) => {
                            let _ = q.reply.send(Err(e));
                        }
                    }
                }
                if let Some(inf) = &mut self.ctx.pubs.inflight
                    && inf.sent_on.is_none()
                    && inf.retry_at.is_none()
                    && m.link.send(&Ctl::Publish { event: inf.event.signed.clone() })
                {
                    inf.sent_on = Some(m.link.id);
                }
            }
            _ => {}
        }
    }
}

impl Ctx {
    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Server keys.
    pub fn keys(&self) -> Option<ServerKeys> { self.replica.lock().keys.clone() }

    /// Current heads.
    pub fn heads(&self) -> Heads { self.replica.lock().state.sync_heads() }

    /// Is `user` a current member of this server?
    pub fn is_member(&self, user: &UserId) -> bool { self.replica.lock().state.is_member(user) }

    /// (MAC valid, current member) for an incoming Hello from `peer`.
    fn check_hello(&self, peer: &UserId, hello: &Ctl) -> (bool, bool) {
        let r = self.replica.lock();
        let Some(keys) = &r.keys else { return (false, false) };
        let server_ok = matches!(hello, Ctl::Hello { server, .. } if *server == self.server);
        let mac = server_ok && build::verify_hello(hello, &keys.hello_key, peer, &self.me);
        (mac, mac && r.state.is_member(peer))
    }

    /// A Hello addressed to `host`.
    fn hello_for(&self, host: &UserId) -> Option<Ctl> {
        let keys = self.keys()?;
        let info = HelloInfo {
            epoch_seen: self.epoch_seen,
            status: self.net.status_for(&self.server, false),
            have: self.net.have_list(),
            clips: self.net.clips(),
        };
        Some(build::hello(&self.me, &self.server, &keys.hello_key, host, info))
    }

    /// Dial `user` (after `delay`) unless already dialing.
    pub fn dial(&mut self, user: UserId, purpose: DialPurpose, delay: Duration) {
        if user == self.me || self.dialing.contains(&user) {
            return;
        }
        let Some(hello) = self.hello_for(&user) else { return };
        self.dialing.insert(user);
        let ep = self.net.endpoint.clone();
        let hint = self.net.hint(&user);
        let tx = self.dial_tx.clone();
        let timeout = self.net.timings.dial;
        self.net.tasks.spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let result = match link::connect(&ep, &user, hint.as_ref(), ALPN_SERVER, timeout).await {
                Ok(conn) => link::exchange(conn, &hello, timeout).await,
                Err(e) => Err(e),
            };
            let _ = tx.send(DialResult { user, purpose, result }).await;
        });
    }

    /// Start a link for a connection whose control stream is open.
    pub fn spawn_link(&mut self, conn: Connection, send: SendStream, reader: FrameReader) -> Link {
        let id = self.id();
        Link::spawn(&self.net.tasks, id, conn, send, reader, self.link_tx.clone())
    }

    /// Validate a Welcome reply (from the join flow) into a member start.
    fn welcome_start(&mut self, d: Box<Dialed>) -> Option<MemberStart> {
        match d.reply.clone() {
            Ctl::Welcome { claim, roster, your_short_id, your_next_seq }
                if claim.verify_for(&self.server).is_ok() && claim.claim.host == crate::addr::remote_user(&d.conn) =>
            {
                Some(MemberStart { link: d, claim, roster, short_id: your_short_id, next_seq: your_next_seq })
            }
            _ => {
                link::close_conn(&d.conn, tincan_core::CLOSE_NORMAL, "bad welcome");
                None
            }
        }
    }

    /// Stream what `theirs` lacks on `link`'s bulk queue; reports `SyncSent` when done.
    pub fn spawn_sync(&self, link: &Link, theirs: Heads) {
        let storage = self.net.storage.clone();
        let replica = self.replica.clone();
        let bulk = link.bulk();
        let tx = self.link_tx.clone();
        let id = link.id;
        self.net.tasks.spawn(async move {
            let n = send_missing(storage, replica, theirs, bulk).await;
            tracing::debug!(link = id, events = n, "sync sent");
            let _ = tx.send(LinkEvent::SyncSent { link: id }).await;
        });
    }

    /// Sign my next event. `seq_hint` is the host's view of my next seq (0 if none).
    pub fn sign_own(&mut self, body: Body, seq_hint: u64) -> Result<StoredEvent> {
        let mut r = self.replica.lock();
        if let Body::Message { channel, .. } = &body
            && !r.state.can_post(&self.me, channel)
        {
            return Err(NetError::Invalid("You can't post in this channel.".into()));
        }
        let seq = r.next_own_seq(&self.me).max(seq_hint);
        let body = match body {
            Body::MemberLeave { .. } => Body::MemberLeave { last_seq: seq },
            b => b,
        };
        let hlc = self.net.hlc.now();
        let signed = build::event(&self.net.identity, self.server, seq, hlc, body).map_err(NetError::from_core_user)?;
        let stored = StoredEvent::from_signed(signed)?;
        if let Verdict::Reject(reason) = r.state.check(&stored.id, &stored.event, now_ms()) {
            return Err(NetError::Invalid(crate::error::capitalize(&reason)));
        }
        r.reserve_own_seq(self.net.storage.as_ref(), seq);
        Ok(stored)
    }

    /// Verify, validate, apply and store events from a peer; emits app events.
    pub fn ingest_signed(&mut self, events: Vec<SignedEvent>) -> Vec<(StoredEvent, Change)> {
        let mut applied = Vec::new();
        {
            let mut r = self.replica.lock();
            for signed in events {
                let stored = match StoredEvent::from_signed(signed) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!(error = %e, "dropping unverifiable event");
                        continue;
                    }
                };
                if stored.conv() != self.server {
                    continue;
                }
                let res = r.ingest(self.net.storage.as_ref(), &self.net.hlc, stored);
                if let Verdict::Reject(reason) = &res.verdict {
                    tracing::debug!(%reason, "event rejected");
                }
                applied.extend(res.applied);
            }
        }
        self.after_applied(&applied);
        applied
    }

    /// Emit app events for applied events; react to my own MemberLeave.
    pub fn after_applied(&self, applied: &[(StoredEvent, Change)]) {
        if !applied.is_empty() {
            self.net.emit_applied(self.server, applied);
            let r = self.replica.lock();
            if r.record.name != r.state.name && !r.state.name.is_empty() {
                drop(r);
                let mut r = self.replica.lock();
                r.record.name = r.state.name.clone();
                if let Err(e) = self.net.storage.upsert_conv(&r.record) {
                    tracing::warn!(error = %e, "updating server name failed");
                }
            }
        }
    }

    /// Record epoch and host in the conversation record.
    fn update_record(&self, epoch: u64, host: UserId) {
        let mut r = self.replica.lock();
        if r.record.last_epoch < epoch || r.record.last_host != Some(host) {
            r.record.last_epoch = r.record.last_epoch.max(epoch);
            r.record.last_host = Some(host);
            if let Err(e) = self.net.storage.upsert_conv(&r.record) {
                tracing::warn!(error = %e, "persisting host epoch failed");
            }
        }
    }

    /// Remember a roster: addresses, peers, succession; tell the app and the DM layer.
    pub fn note_roster(&mut self, roster: &Roster) {
        let now = now_ms();
        let mut seen = Vec::new();
        for e in &roster.online {
            if e.user == self.me {
                continue;
            }
            self.net.learn_hint(&e.user, &e.addr);
            self.persist_peer(e.user, &e.addr, now);
            seen.push(e.user);
        }
        let mut online: Vec<UserId> = roster.online.iter().map(|e| e.user).collect();
        online.sort();
        let sig = (online, roster.succession.clone());
        if self.roster_sig.as_ref() != Some(&sig) {
            self.roster_sig = Some(sig);
            if let Ok(bytes) = postcard::to_stdvec(roster)
                && let Err(e) = self.net.storage.set_kv(&roster_key(&self.server), &bytes)
            {
                tracing::warn!(error = %e, "persisting roster failed");
            }
        }
        self.last_roster = Some(roster.clone());
        *self.shared.roster.lock() = Some(roster.clone());
        self.net.emit(NetEvent::RosterUpdated { server: self.server, roster: roster.clone() });
        self.net.dm.peers_seen(&self.net, &seen);
    }

    /// Persist a peer address (throttled).
    pub fn persist_peer(&mut self, user: UserId, addr: &AddrHint, now: u64) {
        let fresh = match self.persisted.get(&user) {
            Some((a, t)) => a != addr || now.saturating_sub(*t) > PEER_PERSIST_EVERY_MS,
            None => true,
        };
        if fresh {
            self.persisted.insert(user, (addr.clone(), now));
            let rec = PeerRecord { user, addr: addr.clone(), last_seen_ms: now };
            if let Err(e) = self.net.storage.upsert_peer(&self.server, &rec) {
                tracing::warn!(error = %e, "persisting peer failed");
            }
        }
    }

    /// The address to give out for the host.
    fn host_addr(&self, host: &UserId, roster: &Roster) -> AddrHint {
        roster
            .online
            .iter()
            .find(|e| e.user == *host)
            .map(|e| e.addr.clone())
            .or_else(|| self.net.hint(host))
            .unwrap_or_default()
    }

    /// Cold-open candidates: last host, succession, recently seen peers.
    fn cold_candidates(&self) -> Vec<UserId> {
        let mut out = Vec::new();
        if let Some(h) = self.replica.lock().record.last_host {
            out.push(h);
        }
        if let Some(r) = &self.last_roster {
            out.push(r.host);
            out.extend(r.succession.iter().copied());
        }
        match self.net.storage.peers(&self.server, COLD_OPEN_PEERS) {
            Ok(peers) => {
                for p in peers {
                    self.net.learn_hint(&p.user, &p.addr);
                    out.push(p.user);
                }
            }
            Err(e) => tracing::warn!(error = %e, "reading cached peers failed"),
        }
        out
    }

    /// Recompute the connection info from the role; emit if it changed.
    pub fn update_info(&self, role: &Role) {
        let epoch = self.epoch_seen;
        let info = match role {
            Role::Seeking(_) => ConnectionInfo { state: ConnState::Connecting, host: None, path: PathKind::None, rtt_ms: None, epoch },
            Role::Offline { .. } | Role::Stopped => ConnectionInfo { state: ConnState::Offline, host: None, path: PathKind::None, rtt_ms: None, epoch },
            Role::Member(m) => ConnectionInfo {
                state: ConnState::Member,
                host: Some(m.host),
                path: if m.path.direct {
                    PathKind::Direct
                } else if m.path.relay {
                    PathKind::Relay
                } else {
                    PathKind::None
                },
                rtt_ms: m.path.rtt_ms,
                epoch: m.claim.claim.epoch,
            },
            Role::Host(h) => h.info(self.me),
        };
        self.publish_info(info);
    }

    /// Store `info`; emit `ConnectionChanged` if it changed materially.
    pub fn publish_info(&self, info: ConnectionInfo) {
        let changed = {
            let mut cur = self.shared.info.lock();
            let material = cur.state != info.state
                || cur.host != info.host
                || cur.path != info.path
                || cur.epoch != info.epoch
                || rtt_moved(cur.rtt_ms, info.rtt_ms);
            if material {
                *cur = info.clone();
            }
            material
        };
        if changed {
            self.net.emit(NetEvent::ConnectionChanged { server: self.server, info });
        }
    }
}

/// RTT changed enough to tell the UI (25% and at least 10 ms, or appeared/disappeared).
fn rtt_moved(a: Option<u32>, b: Option<u32>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => {
            let d = x.abs_diff(y);
            d >= 10 && u64::from(d) * 4 >= u64::from(x.max(1))
        }
        (None, None) => false,
        _ => true,
    }
}

/// Lowest short id not in use (1-based).
pub(super) fn alloc_short_id(used: &HashMap<UserId, u16>) -> u16 {
    let taken: HashSet<u16> = used.values().copied().collect();
    (1..=tincan_core::media::MAX_SHORT_ID).find(|i| !taken.contains(i)).unwrap_or(tincan_core::media::MAX_SHORT_ID)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ids_fill_gaps() {
        let mut m = HashMap::new();
        assert_eq!(alloc_short_id(&m), 1);
        m.insert(UserId([1; 32]), 1);
        m.insert(UserId([2; 32]), 3);
        assert_eq!(alloc_short_id(&m), 2);
    }

    #[test]
    fn rtt_threshold() {
        assert!(!rtt_moved(Some(100), Some(105)));
        assert!(rtt_moved(Some(100), Some(130)));
        assert!(!rtt_moved(Some(2), Some(8)));
        assert!(rtt_moved(None, Some(8)));
    }
}
