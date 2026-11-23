//! Host role: accept members, validate and fan out events, roster, liveness, holders,
//! clean handoff, proactive migration and partition merge.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tincan_core::build::{self, JoinRejection};
use tincan_core::election::{self, Candidate, HostSide, SignedHostClaim, Status};
use tincan_core::event::{Body, SignedEvent, StoredEvent};
use tincan_core::media::{KIND_SCREEN, track_id};
use tincan_core::state::{Change, Verdict};
use tincan_core::sync::SYNC_PAGE_BYTES;
use tincan_core::wire::{AddrHint, ClipInfo, Ctl, RejectCode, Roster, RosterEntry};
use tincan_core::{BlobHash, EventId, UserId, now_ms};
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::{Ctx, MemberStart, Next, Role, SeekKind, Session, alloc_short_id};
use crate::addr::{PathSnapshot, path_snapshot};
use crate::error::{NetError, Result};
use crate::events::{ConnState, ConnectionInfo, NetEvent, PathKind};
use crate::hooks::{CtlSource, is_extra};
use crate::link::{Link, LinkEvent, encode};
use crate::media::HostMedia;
use crate::power;

/// A connected member, as the host sees it.
pub(crate) struct MLink {
    /// Control link.
    pub link: Link,
    /// Short id.
    pub short_id: u16,
    /// Online since (host clock).
    pub since_ms: u64,
    /// Reported status.
    pub status: Status,
    /// When `status` was reported (uptime extrapolation).
    pub status_at: Instant,
    /// Complete blobs held.
    pub have: HashSet<BlobHash>,
    /// Soundboard library.
    pub clips: Vec<ClipInfo>,
    /// Path to the member.
    pub path: PathSnapshot,
    /// Last frame from the member.
    pub last_rx: Instant,
    /// Last time the host replied to a SyncHave with its own.
    pub last_sync_reply: Option<Instant>,
}

/// Partition merge in progress.
pub(super) enum Merge {
    /// I dialed another host and sent HostCompare; waiting for its HostCompare.
    Probing { link: Link, peer: UserId, sent: HostSide, deadline: Instant },
    /// I lost as the prober; waiting for the winner's HostMoved on the probe link.
    AwaitProbe { link: Link, peer: UserId, deadline: Instant },
    /// I lost to a host that probed me; waiting for its HostMoved on its member link.
    AwaitMember { peer: UserId, deadline: Instant },
}

impl Merge {
    fn deadline(&self) -> Instant {
        match self {
            Merge::Probing { deadline, .. } | Merge::AwaitProbe { deadline, .. } | Merge::AwaitMember { deadline, .. } => *deadline,
        }
    }
    fn link_id(&self) -> Option<u64> {
        match self {
            Merge::Probing { link, .. } | Merge::AwaitProbe { link, .. } => Some(link.id),
            Merge::AwaitMember { .. } => None,
        }
    }
}

/// What to do after a clean handoff.
pub(crate) enum AfterHandoff {
    /// End the session (app exit, leave) and then reply.
    Stop(oneshot::Sender<()>),
    /// Stay in the server as a member of the new host (proactive migration).
    Member,
}

/// A clean handoff in progress.
pub(super) struct Handoff {
    target: UserId,
    new_claim: Option<(SignedHostClaim, AddrHint)>,
    deadline: Instant,
    then: AfterHandoff,
}

/// Proactive migration hysteresis.
#[derive(Debug)]
pub(crate) struct Proactive {
    better_since: Option<Instant>,
    last_migration: Instant,
}

impl Proactive {
    /// Start counting from `became_host`.
    pub fn new(became_host: Instant) -> Self { Self { better_since: None, last_migration: became_host } }

    /// True if a proactive migration should start now: no voice room active, `best` beats
    /// `current` by the margin continuously for `hold`, and `interval` passed since the last
    /// migration.
    pub fn evaluate(&mut self, now: Instant, voice_active: bool, best: Option<u32>, current: u32, hold: Duration, interval: Duration) -> bool {
        if voice_active {
            self.better_since = None;
            return false;
        }
        match best {
            Some(b) if election::beats_by_margin(b, current) => {
                let since = *self.better_since.get_or_insert(now);
                now.duration_since(since) >= hold && now.duration_since(self.last_migration) >= interval
            }
            _ => {
                self.better_since = None;
                false
            }
        }
    }
}

/// State while hosting.
pub(crate) struct HostState {
    /// My current claim.
    pub claim: SignedHostClaim,
    /// Connected members.
    pub links: HashMap<UserId, MLink>,
    /// Joiners that got JoinAccepted and must send Hello next (by link id).
    pub joiners: HashMap<u64, Link>,
    /// Links kept only until the peer closes them.
    pub retiring: Vec<Link>,
    /// Short ids, inherited and assigned.
    pub short_ids: HashMap<UserId, u16>,
    rev: u32,
    roster_at: Option<Instant>,
    since_ms: u64,
    alone_since: Option<Instant>,
    probe_at: Instant,
    pub(super) merge: Option<Merge>,
    pub(super) handoff: Option<Handoff>,
    last_vv: Instant,
    last_proactive: Instant,
    proactive: Proactive,
    /// SFU and rendezvous publisher of this host role (torn down when the state drops).
    _media: HostMedia,
}

impl HostState {
    /// Fresh host state (no members yet).
    pub(super) fn new(ctx: &Ctx, claim: SignedHostClaim, mut short_ids: HashMap<UserId, u16>) -> Self {
        if !short_ids.contains_key(&ctx.me) {
            let id = alloc_short_id(&short_ids);
            short_ids.insert(ctx.me, id);
        }
        let my_short = short_ids.get(&ctx.me).copied().unwrap_or(0);
        let rdv = ctx.keys().and_then(|k| crate::rdv::spawn_host_runner(&ctx.net, ctx.server, k, ctx.shared.clone()));
        let media = ctx.net.media.host_started(&ctx.net, ctx.server, claim.claim.epoch, my_short, rdv);
        let now = Instant::now();
        Self {
            claim,
            links: HashMap::new(),
            joiners: HashMap::new(),
            retiring: Vec::new(),
            short_ids,
            rev: 0,
            roster_at: None,
            since_ms: now_ms(),
            alone_since: Some(now),
            probe_at: now + jitter(ctx.net.timings.solo_retry),
            merge: None,
            handoff: None,
            last_vv: now,
            last_proactive: now,
            proactive: Proactive::new(now),
            _media: media,
        }
    }

    /// Every link this state holds.
    pub(super) fn into_links(self) -> Vec<Link> {
        let mut out: Vec<Link> = self.links.into_values().map(|m| m.link).collect();
        out.extend(self.joiners.into_values());
        out.extend(self.retiring);
        match self.merge {
            Some(Merge::Probing { link, .. }) | Some(Merge::AwaitProbe { link, .. }) => out.push(link),
            _ => {}
        }
        out
    }

    /// Timer deadlines of this role.
    pub(super) fn wakeups(&self) -> Vec<Instant> {
        let mut v = vec![self.probe_at];
        v.extend(self.roster_at);
        v.extend(self.merge.as_ref().map(|m| m.deadline()));
        v.extend(self.handoff.as_ref().map(|h| h.deadline));
        v
    }

    /// Connection info for this role.
    pub(super) fn info(&self, me: UserId) -> ConnectionInfo {
        ConnectionInfo {
            state: if self.links.is_empty() { ConnState::Solo } else { ConnState::Hosting },
            host: Some(me),
            path: PathKind::None,
            rtt_ms: None,
            epoch: self.claim.claim.epoch,
        }
    }

    fn user_of(&self, link: u64) -> Option<UserId> { self.links.iter().find(|(_, m)| m.link.id == link).map(|(u, _)| *u) }

    /// Schedule a roster broadcast (debounced).
    pub(super) fn mark_roster(&mut self, ctx: &Ctx) {
        if self.roster_at.is_none() {
            self.roster_at = Some(Instant::now() + ctx.net.timings.roster_debounce);
        }
    }

    /// Send a frame to every member (except one). Members whose queue is full are dropped
    /// (their link is closed; they reconnect and resync).
    pub(super) fn broadcast(&self, ctx: &Ctx, ctl: &Ctl, except: Option<UserId>) {
        let Some(bytes) = encode(ctl) else { return };
        for (u, m) in &self.links {
            if Some(*u) == except {
                continue;
            }
            if !m.link.send_bytes(bytes.clone()) {
                tracing::info!(server = %ctx.server.short(), member = %u.short(), "member too slow; dropping");
                m.link.close(tincan_core::CLOSE_NORMAL, "too slow");
            }
        }
    }

    /// Fan out applied events to every member except `except`.
    pub(super) fn fanout(&self, ctx: &Ctx, applied: &[(StoredEvent, Change)], except: Option<UserId>) {
        for page in pages(applied.iter().map(|(e, _)| &e.signed)) {
            self.broadcast(ctx, &page, except);
        }
    }

    /// Online holders of `hash`. `requester` = the member asking (None: I am asking).
    pub(super) fn holders(&self, ctx: &Ctx, hash: &BlobHash, requester: Option<UserId>) -> Vec<(UserId, AddrHint)> {
        let mut out = Vec::new();
        if requester.is_some() && ctx.net.holds(hash) {
            out.push((ctx.me, ctx.net.my_hint()));
        }
        for (u, m) in &self.links {
            if Some(*u) == requester {
                continue;
            }
            if m.have.contains(hash) || m.clips.iter().any(|c| c.hash == *hash) {
                out.push((*u, m.path.remote.clone()));
            }
        }
        out
    }

    /// Election candidates (members only).
    fn candidates(&self) -> Vec<Candidate> {
        self.links
            .iter()
            .map(|(u, m)| Candidate {
                user: *u,
                direct: m.path.direct,
                ac_power: m.status.ac_power,
                uptime_s: m.status.uptime_s.saturating_add(u32::try_from(m.status_at.elapsed().as_secs()).unwrap_or(u32::MAX)),
                rtt_ms: m.path.rtt_ms.unwrap_or(1000),
                hosting_ok: m.status.hosting_ok,
                synced: m.status.synced,
            })
            .collect()
    }

    /// My own election score as host.
    fn my_score(&self, ctx: &Ctx) -> u32 {
        let direct = self.links.values().any(|l| l.path.direct) || !ctx.net.my_hint().direct.is_empty();
        election::score(&Candidate {
            user: ctx.me,
            direct,
            ac_power: power::ac_power(),
            uptime_s: power::uptime_s(),
            rtt_ms: 0,
            hosting_ok: ctx.net.allow_hosting(),
            synced: true,
        })
    }

    /// My side of a partition-merge comparison.
    fn my_side(&self, ctx: &Ctx, exclude: Option<UserId>) -> HostSide {
        let connected = self.links.keys().filter(|u| Some(**u) != exclude).count();
        HostSide { host: ctx.me, epoch: self.claim.claim.epoch, connected: u32::try_from(connected).unwrap_or(u32::MAX), score: self.my_score(ctx) }
    }

    /// The roster as of now. `bump` increments the revision (broadcasts do, Welcome does not).
    pub(super) fn build_roster(&mut self, ctx: &Ctx, bump: bool) -> Roster {
        if bump {
            self.rev = self.rev.wrapping_add(1);
        }
        let status = ctx.net.status_for(&ctx.server, true);
        let my_short = self.short_ids.get(&ctx.me).copied().unwrap_or(0);
        let mut online = vec![RosterEntry {
            user: ctx.me,
            short_id: my_short,
            addr: ctx.net.my_hint(),
            since_ms: self.since_ms,
            voice: status.voice,
            muted: status.muted,
            deafened: status.deafened,
            sharing: status.sharing.then(|| track_id(my_short, KIND_SCREEN)),
            direct: true,
            rtt_ms: 0,
        }];
        let mut members: Vec<&MLink> = self.links.values().collect();
        members.sort_by_key(|m| m.short_id);
        for m in members {
            online.push(RosterEntry {
                user: m.link.peer,
                short_id: m.short_id,
                addr: m.path.remote.clone(),
                since_ms: m.since_ms,
                voice: m.status.voice,
                muted: m.status.muted,
                deafened: m.status.deafened,
                sharing: m.status.sharing.then(|| track_id(m.short_id, KIND_SCREEN)),
                direct: m.path.direct,
                rtt_ms: m.path.rtt_ms.map(|r| u16::try_from(r).unwrap_or(u16::MAX)).unwrap_or(0),
            });
        }
        Roster {
            epoch: self.claim.claim.epoch,
            rev: self.rev,
            host: ctx.me,
            online,
            succession: election::succession(&self.candidates()),
        }
    }

    /// Accept a member whose Hello was validated.
    pub(super) fn accept_member(&mut self, ctx: &mut Ctx, link: Link, hello: Ctl) {
        let Ctl::Hello { status, have, clips, epoch_seen, .. } = hello else {
            link.close(tincan_core::CLOSE_REJECTED, "expected hello");
            return;
        };
        let peer = link.peer;
        if let Some(old) = self.links.remove(&peer) {
            old.link.close(tincan_core::CLOSE_NORMAL, "replaced");
            ctx.net.media.member_down(ctx.server, peer, old.link.conn.stable_id());
            ctx.net.emit(NetEvent::MemberLinkDown { server: ctx.server, user: peer });
        }
        let short_id = match self.short_ids.get(&peer) {
            Some(id) => *id,
            None => {
                let id = alloc_short_id(&self.short_ids);
                self.short_ids.insert(peer, id);
                id
            }
        };
        if epoch_seen > self.claim.claim.epoch {
            tracing::debug!(member = %peer.short(), epoch_seen, mine = self.claim.claim.epoch, "member saw a newer epoch");
        }
        let path = path_snapshot(&link.conn);
        ctx.net.learn_hint(&peer, &path.remote);
        ctx.persist_peer(peer, &path.remote, now_ms());
        let conn = link.conn.clone();
        let live = link.live();
        let now = Instant::now();
        self.links.insert(
            peer,
            MLink {
                link,
                short_id,
                since_ms: now_ms(),
                status,
                status_at: now,
                have: have.into_iter().collect(),
                clips,
                path,
                last_rx: now,
                // The Welcome below is followed by my SyncHave already.
                last_sync_reply: Some(now),
            },
        );
        let roster = self.build_roster(ctx, false);
        let next_seq = ctx.replica.lock().state.next_seq(&peer);
        let heads = ctx.heads();
        if let Some(m) = self.links.get(&peer) {
            let _ = m.link.send(&Ctl::Welcome { claim: self.claim.clone(), roster, your_short_id: short_id, your_next_seq: next_seq });
            let _ = m.link.send(&Ctl::SyncHave { heads });
        }
        ctx.shared.member_conns.lock().insert(peer, (short_id, conn.clone()));
        ctx.net.media.member_up(ctx.server, peer, short_id, conn, live);
        ctx.net.emit(NetEvent::MemberLinkUp { server: ctx.server, user: peer, short_id });
        tracing::info!(server = %ctx.server.short(), member = %peer.short(), short_id, "member connected");
        self.alone_since = None;
        self.mark_roster(ctx);
        if ctx.tick_at.is_none() {
            ctx.tick_at = Some(now + ctx.net.timings.tick);
        }
        ctx.publish_info(self.info(ctx.me));
        ctx.net.dm.peers_seen(&ctx.net, &[peer]);
    }

    /// Remove a member.
    fn drop_member(&mut self, ctx: &Ctx, user: UserId, why: &str) {
        if let Some(m) = self.links.remove(&user) {
            tracing::info!(server = %ctx.server.short(), member = %user.short(), why, "member gone");
            m.link.close(tincan_core::CLOSE_NORMAL, "gone");
            ctx.shared.member_conns.lock().remove(&user);
            ctx.net.media.member_down(ctx.server, user, m.link.conn.stable_id());
            ctx.net.emit(NetEvent::MemberLinkDown { server: ctx.server, user });
            self.mark_roster(ctx);
            if self.links.is_empty() {
                self.alone_since = Some(Instant::now());
            }
            ctx.publish_info(self.info(ctx.me));
        }
    }

    /// Validate, store and fan out an event authored here (mine, or a joiner's admission).
    pub(super) fn publish_local(&mut self, ctx: &mut Ctx, ev: StoredEvent) -> Result<StoredEvent> {
        let verdict = ctx.replica.lock().state.check(&ev.id, &ev.event, now_ms());
        match verdict {
            Verdict::Accept => {
                let res = {
                    let mut r = ctx.replica.lock();
                    r.ingest(ctx.net.storage.as_ref(), &ctx.net.hlc, ev.clone())
                };
                ctx.after_applied(&res.applied);
                self.fanout(ctx, &res.applied, None);
                Ok(ev)
            }
            Verdict::Stale => {
                if matches!(ctx.net.storage.event(&ev.id), Ok(Some(_))) {
                    Ok(ev)
                } else {
                    Err(NetError::Invalid("That conflicts with an earlier change.".into()))
                }
            }
            Verdict::Gap | Verdict::Future => {
                ctx.replica.lock().release_own_seq(ctx.net.storage.as_ref(), ev.event.seq);
                Err(NetError::Invalid("That is not possible yet. Try again in a moment.".into()))
            }
            Verdict::Reject(reason) => {
                ctx.replica.lock().release_own_seq(ctx.net.storage.as_ref(), ev.event.seq);
                Err(NetError::Invalid(crate::error::capitalize(&reason)))
            }
        }
    }

    /// Handle a JoinRequest: verify, admit, store and fan out; returns JoinAccepted.
    pub(super) fn admit_joiner(&mut self, ctx: &mut Ctx, peer: &UserId, ctl: &Ctl) -> std::result::Result<Ctl, JoinRejection> {
        let internal = |why: &str| JoinRejection { code: RejectCode::Internal, reason: why.to_string() };
        let verified = {
            let r = ctx.replica.lock();
            build::verify_join_request(ctl, &r.state, &ctx.me, now_ms())?
        };
        if verified.joiner != *peer {
            return Err(JoinRejection { code: RejectCode::BadInvite, reason: "join request from someone else".into() });
        }
        let secret = ctx.replica.lock().record.server_secret.ok_or_else(|| internal("server secret missing"))?;
        let genesis = match ctx.net.storage.event(&EventId(ctx.server.0)) {
            Ok(Some(g)) => g.signed,
            _ => return Err(internal("genesis missing")),
        };
        let admit: SignedEvent = if verified.already_member {
            let found = ctx.net.storage.state_events(&ctx.server).ok().and_then(|evs| {
                evs.into_iter()
                    .rev()
                    .find(|e| matches!(&e.event.body, Body::MemberAdmit { user, .. } if *user == verified.joiner))
            });
            found.map(|e| e.signed).ok_or_else(|| internal("admission not found"))?
        } else {
            let ev = {
                let mut r = ctx.replica.lock();
                let seq = r.next_own_seq(&ctx.me);
                let hlc = ctx.net.hlc.now();
                let signed = build::admit(&ctx.net.identity, &r.state, seq, hlc, &verified)
                    .map_err(|e| JoinRejection { code: RejectCode::BadInvite, reason: e.to_string() })?;
                let stored = StoredEvent::from_signed(signed).map_err(|e| internal(&e.to_string()))?;
                r.reserve_own_seq(ctx.net.storage.as_ref(), seq);
                stored
            };
            self.publish_local(ctx, ev.clone()).map_err(|e| internal(&e.to_string()))?;
            let res = {
                let mut r = ctx.replica.lock();
                r.ingest(ctx.net.storage.as_ref(), &ctx.net.hlc, verified.profile.clone())
            };
            ctx.after_applied(&res.applied);
            self.fanout(ctx, &res.applied, None);
            tracing::info!(server = %ctx.server.short(), joiner = %peer.short(), "admitted a new member");
            ev.signed
        };
        Ok(build::join_accepted(&secret, &genesis, &admit))
    }

    /// Begin a clean handoff to the best connected successor.
    pub(super) fn start_handoff(&mut self, ctx: &mut Ctx, then: AfterHandoff) -> Next {
        if self.handoff.is_some() {
            if let AfterHandoff::Stop(reply) = then {
                // A handoff is already running; this caller just stops with it.
                let _ = reply.send(());
            }
            return Next::Stay;
        }
        let roster = self.build_roster(ctx, false);
        let target = roster.succession.iter().copied().find(|u| self.links.contains_key(u));
        match target {
            Some(t) => {
                if let Some(m) = self.links.get(&t) {
                    let _ = m.link.send(&Ctl::Handoff { claim: self.claim.clone(), roster });
                }
                tracing::info!(server = %ctx.server.short(), to = %t.short(), "handing off");
                self.handoff = Some(Handoff { target: t, new_claim: None, deadline: Instant::now() + ctx.net.timings.handoff_ack, then });
                Next::Stay
            }
            None => {
                self.handoff = Some(Handoff { target: ctx.me, new_claim: None, deadline: Instant::now(), then });
                Next::Stay
            }
        }
    }
}

/// `d` scaled by a random factor in [0.8, 1.2), so peers' periodic probes do not stay in
/// lockstep.
fn jitter(d: Duration) -> Duration {
    use rand::Rng;
    let permille: u32 = rand::rng().random_range(800..1200);
    d * permille / 1000
}

/// Split events into `Events` frames of at most `SYNC_PAGE_BYTES`.
fn pages<'a>(events: impl Iterator<Item = &'a SignedEvent>) -> Vec<Ctl> {
    let mut out = Vec::new();
    let mut page = Vec::new();
    let mut bytes = 0usize;
    for e in events {
        bytes += e.bytes.len() + 80;
        page.push(e.clone());
        if bytes >= SYNC_PAGE_BYTES {
            out.push(Ctl::Events { events: std::mem::take(&mut page), more: false });
            bytes = 0;
        }
    }
    if !page.is_empty() {
        out.push(Ctl::Events { events: page, more: false });
    }
    out
}

/// Queue `moved` (if any) on every link except `skip`, then close them all with `code`
/// once each peer closed or after one second. `done` fires at the end.
fn flush_and_close(ctx: &Ctx, links: Vec<Link>, moved: Option<Ctl>, skip: Option<UserId>, code: u32, done: Option<oneshot::Sender<()>>) {
    for l in &links {
        if Some(l.peer) == skip {
            l.close(code, "handoff done");
            continue;
        }
        if let Some(m) = &moved {
            let _ = l.send(m);
        }
    }
    ctx.net.tasks.spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(1);
        for l in &links {
            let _ = tokio::time::timeout_at(deadline, l.conn.closed()).await;
        }
        for l in &links {
            l.close(code, "host leaving");
        }
        drop(links);
        if let Some(d) = done {
            let _ = d.send(());
        }
    });
}

/// Finish (or give up) a handoff and leave the host role.
fn finish_handoff(s: &mut Session) -> Next {
    let Role::Host(h) = std::mem::replace(&mut s.role, Role::Stopped) else { return Next::Stay };
    let mut h = *h;
    let Some(ho) = h.handoff.take() else {
        s.role = Role::Host(Box::new(h));
        return Next::Stay;
    };
    let epoch = h.claim.claim.epoch;
    let moved = ho.new_claim.as_ref().map(|(c, a)| Ctl::HostMoved { claim: c.clone(), addr: a.clone() });
    let users: Vec<UserId> = h.links.keys().copied().collect();
    for u in &users {
        s.ctx.net.emit(NetEvent::MemberLinkDown { server: s.ctx.server, user: *u });
    }
    s.ctx.shared.member_conns.lock().clear();
    let links = h.into_links();
    let target = ho.target;
    let acked = ho.new_claim.is_some();
    tracing::info!(server = %s.ctx.server.short(), acked, "leaving the host role");
    match ho.then {
        AfterHandoff::Stop(reply) => {
            flush_and_close(&s.ctx, links, moved, Some(target), tincan_core::CLOSE_HOST_LEAVING, Some(reply));
            Next::Stop
        }
        AfterHandoff::Member => {
            flush_and_close(&s.ctx, links, moved, Some(target), tincan_core::CLOSE_HOST_LEAVING, None);
            if target == s.ctx.me {
                return Next::Seek(SeekKind::ColdOpen, None);
            }
            Next::Seek(
                SeekKind::Failover { failed_host: s.ctx.me, failed_epoch: epoch, succession: vec![target], rank: None },
                Some(target),
            )
        }
    }
}

/// A dial while hosting reached another host of this server: start the merge as prober.
pub(super) fn start_probe_merge(s: &mut Session, peer: UserId, start: MemberStart) -> Next {
    let Role::Host(h) = &mut s.role else { return Next::Stay };
    // Both hosts may probe each other at once; then each is also the other's member. Only the
    // probe dialed by the lower user id goes on, the other side backs off.
    let crossed = h.links.contains_key(&peer) && peer < s.ctx.me;
    let busy = h.merge.is_some() || h.handoff.is_some() || crossed;
    let d = start.link;
    if busy || start.claim.claim.host == s.ctx.me {
        crate::link::close_conn(&d.conn, tincan_core::CLOSE_NORMAL, "busy");
        return Next::Stay;
    }
    let d = *d;
    let link = s.ctx.spawn_link(d.conn, d.send, d.reader);
    let sent = h.my_side(&s.ctx, Some(peer));
    tracing::info!(server = %s.ctx.server.short(), other = %peer.short(), "found another host: comparing");
    let _ = link.send(&Ctl::HostCompare { claim: h.claim.clone(), connected: sent.connected, score: sent.score });
    let _ = link.send(&Ctl::SyncHave { heads: s.ctx.heads() });
    h.merge = Some(Merge::Probing { link, peer, sent, deadline: Instant::now() + s.ctx.net.timings.merge_timeout });
    Next::Stay
}

/// I won a merge: claim a new epoch above both and tell everyone.
fn win_merge(s: &mut Session, other_epoch: u64) -> Option<(SignedHostClaim, AddrHint)> {
    let Role::Host(h) = &mut s.role else { return None };
    let epoch = h.claim.claim.epoch.max(other_epoch).max(s.ctx.epoch_seen) + 1;
    let claim = match build::host_claim(&s.ctx.net.identity, &s.ctx.server, epoch, Some(s.ctx.me), now_ms()) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "cannot sign host claim");
            return None;
        }
    };
    h.claim = claim.clone();
    s.ctx.epoch_seen = epoch;
    s.ctx.update_record(epoch, s.ctx.me);
    let addr = s.ctx.net.my_hint();
    // My own members just refresh their claim (same host, new epoch).
    h.broadcast(&s.ctx, &Ctl::HostMoved { claim: claim.clone(), addr: addr.clone() }, None);
    h.mark_roster(&s.ctx);
    s.ctx.publish_info(h.info(s.ctx.me));
    tracing::info!(server = %s.ctx.server.short(), epoch, "won the merge");
    Some((claim, addr))
}

/// I lost a merge: send my members to the winner and close their links.
fn lose_merge_links(s: &mut Session, claim: &SignedHostClaim, addr: &AddrHint) {
    let Role::Host(h) = std::mem::replace(&mut s.role, Role::Stopped) else { return };
    let h = *h;
    for u in h.links.keys() {
        s.ctx.net.emit(NetEvent::MemberLinkDown { server: s.ctx.server, user: *u });
    }
    s.ctx.shared.member_conns.lock().clear();
    let rest = h.into_links();
    let moved = Ctl::HostMoved { claim: claim.clone(), addr: addr.clone() };
    flush_and_close(&s.ctx, rest, Some(moved), None, tincan_core::CLOSE_REDIRECT, None);
    s.ctx.tick_at = None;
    tracing::info!(server = %s.ctx.server.short(), winner = %claim.claim.host.short(), "lost the merge");
}

pub(super) fn on_link(s: &mut Session, ev: LinkEvent) -> Next {
    let Role::Host(h) = &mut s.role else { return Next::Stay };
    match ev {
        LinkEvent::SyncSent { .. } => Next::Stay,
        LinkEvent::Closed { link, .. } => {
            if h.joiners.remove(&link).is_some() {
                return Next::Stay;
            }
            if let Some(pos) = h.retiring.iter().position(|l| l.id == link) {
                h.retiring.swap_remove(pos);
                return Next::Stay;
            }
            if h.merge.as_ref().and_then(|m| m.link_id()) == Some(link) {
                h.merge = None;
                return Next::Stay;
            }
            if let Some(user) = h.user_of(link) {
                if h.handoff.as_ref().is_some_and(|ho| ho.target == user) {
                    h.drop_member(&s.ctx, user, "closed during handoff");
                    return finish_handoff(s);
                }
                if matches!(&h.merge, Some(Merge::AwaitMember { peer, .. }) if *peer == user) {
                    h.merge = None;
                }
                h.drop_member(&s.ctx, user, "closed");
            }
            Next::Stay
        }
        LinkEvent::Frame { link, ctl } => {
            let ctl = *ctl;
            if let Some(j) = h.joiners.remove(&link) {
                let (mac_ok, member) = s.ctx.check_hello(&j.peer, &ctl);
                if mac_ok && member && h.handoff.is_none() {
                    h.accept_member(&mut s.ctx, j, ctl);
                } else {
                    j.close(tincan_core::CLOSE_REJECTED, "expected a valid hello");
                }
                return Next::Stay;
            }
            if h.merge.as_ref().and_then(|m| m.link_id()) == Some(link) {
                return probe_frame(s, ctl);
            }
            if h.retiring.iter().any(|l| l.id == link) {
                return Next::Stay;
            }
            match h.user_of(link) {
                Some(user) => member_frame(s, user, ctl),
                None => Next::Stay,
            }
        }
    }
}

fn member_frame(s: &mut Session, user: UserId, ctl: Ctl) -> Next {
    let Role::Host(h) = &mut s.role else { return Next::Stay };
    let now = Instant::now();
    let Some(m) = h.links.get_mut(&user) else { return Next::Stay };
    // Liveness follows the member's session actor, not its media tasks.
    if !is_extra(&ctl) {
        m.last_rx = now;
    }
    match ctl {
        Ctl::Publish { event } => {
            let (ack, applied) = member_publish(&mut s.ctx, user, event);
            if let Some(m) = h.links.get(&user) {
                let _ = m.link.send(&ack);
            }
            if let Some(((_, _), rest)) = applied.split_first() {
                h.fanout(&s.ctx, &applied[..1], Some(user));
                if !rest.is_empty() {
                    h.fanout(&s.ctx, rest, None);
                }
            }
            Next::Stay
        }
        Ctl::Events { events, .. } => {
            let applied = s.ctx.ingest_signed(events);
            h.fanout(&s.ctx, &applied, Some(user));
            Next::Stay
        }
        Ctl::SyncHave { heads } => {
            s.ctx.spawn_sync(&m.link, heads.clone());
            let mine = s.ctx.heads();
            let interval = s.ctx.net.timings.vv_digest;
            if !heads.missing_in(&mine).is_empty() && m.last_sync_reply.is_none_or(|t| t.elapsed() >= interval / 2) {
                m.last_sync_reply = Some(now);
                let _ = m.link.send(&Ctl::SyncHave { heads: mine });
            }
            Next::Stay
        }
        Ctl::Status(st) => {
            m.status = st;
            m.status_at = now;
            h.mark_roster(&s.ctx);
            Next::Stay
        }
        Ctl::Tick { .. } => Next::Stay,
        Ctl::Have { add, remove } => {
            for a in add {
                m.have.insert(a);
            }
            for r in remove {
                m.have.remove(&r);
            }
            Next::Stay
        }
        Ctl::WhoHas { hash } => {
            let holders = h.holders(&s.ctx, &hash, Some(user));
            if let Some(m) = h.links.get(&user) {
                let _ = m.link.send(&Ctl::Holders { hash, holders });
            }
            Next::Stay
        }
        Ctl::Typing { channel, by } => {
            if by == user {
                h.broadcast(&s.ctx, &Ctl::Typing { channel, by }, Some(user));
                s.ctx.net.emit(NetEvent::Typing { conv: s.ctx.server, channel, user });
            }
            Next::Stay
        }
        Ctl::HostCompare { claim, connected, score } => compare_from_member(s, user, claim, connected, score),
        Ctl::HostMoved { claim, addr } => {
            let valid = claim.verify_for(&s.ctx.server).is_ok() && claim.claim.host == user && claim.claim.epoch > h.claim.claim.epoch;
            if !valid {
                return Next::Stay;
            }
            if let Some(ho) = &mut h.handoff
                && ho.target == user
            {
                ho.new_claim = Some((claim, addr));
                return Next::Stay;
            }
            if matches!(&h.merge, Some(Merge::AwaitMember { peer, .. }) if *peer == user) {
                s.ctx.net.learn_hint(&user, &addr);
                s.ctx.epoch_seen = s.ctx.epoch_seen.max(claim.claim.epoch);
                let failed_epoch = h.claim.claim.epoch;
                lose_merge_links(s, &claim, &addr);
                return Next::Seek(
                    SeekKind::Failover { failed_host: s.ctx.me, failed_epoch, succession: vec![user], rank: None },
                    Some(user),
                );
            }
            Next::Stay
        }
        Ctl::HandoffAck { .. } => {
            if h.handoff.as_ref().is_some_and(|ho| ho.target == user) {
                return finish_handoff(s);
            }
            Next::Stay
        }
        other if is_extra(&other) => {
            let short_id = m.short_id;
            s.ctx.net.extra(CtlSource::FromMember { server: s.ctx.server, member: user, short_id }, other);
            Next::Stay
        }
        other => {
            tracing::trace!(?other, "host: ignoring frame");
            Next::Stay
        }
    }
}

/// Validate a member's Publish; returns the ack and the applied events.
fn member_publish(ctx: &mut Ctx, from: UserId, signed: SignedEvent) -> (Ctl, Vec<(StoredEvent, Change)>) {
    let id = signed.id();
    let ack = |ok: bool, reason: &str| Ctl::PublishAck { id, ok, reason: reason.to_string() };
    let stored = match StoredEvent::from_signed(signed) {
        Ok(s) => s,
        Err(_) => return (ack(false, "bad signature"), Vec::new()),
    };
    if stored.event.author != from {
        return (ack(false, "author is not the sender"), Vec::new());
    }
    if stored.conv() != ctx.server {
        return (ack(false, "event for another server"), Vec::new());
    }
    let verdict = ctx.replica.lock().state.check(&id, &stored.event, now_ms());
    match verdict {
        Verdict::Accept => {
            let res = {
                let mut r = ctx.replica.lock();
                r.ingest(ctx.net.storage.as_ref(), &ctx.net.hlc, stored)
            };
            ctx.after_applied(&res.applied);
            (ack(true, ""), res.applied)
        }
        Verdict::Stale => {
            let have = matches!(ctx.net.storage.event(&id), Ok(Some(_)));
            (ack(have, if have { "" } else { "conflicts with an earlier event" }), Vec::new())
        }
        Verdict::Gap | Verdict::Future => (ack(false, "gap"), Vec::new()),
        Verdict::Reject(reason) => (ack(false, &reason), Vec::new()),
    }
}

/// Another host (connected to me as a member) wants to compare.
fn compare_from_member(s: &mut Session, user: UserId, claim: SignedHostClaim, connected: u32, score: u32) -> Next {
    let Role::Host(h) = &mut s.role else { return Next::Stay };
    if h.merge.is_some() || h.handoff.is_some() || claim.verify_for(&s.ctx.server).is_err() || claim.claim.host != user {
        return Next::Stay;
    }
    let theirs = HostSide { host: user, epoch: claim.claim.epoch, connected, score };
    let sent = h.my_side(&s.ctx, Some(user));
    if let Some(m) = h.links.get(&user) {
        let _ = m.link.send(&Ctl::HostCompare { claim: h.claim.clone(), connected: sent.connected, score: sent.score });
    }
    s.ctx.epoch_seen = s.ctx.epoch_seen.max(claim.claim.epoch);
    if election::merge_winner(&sent, &theirs) {
        // `win_merge` broadcasts the new claim to every member, the loser included.
        let _ = win_merge(s, theirs.epoch);
    } else {
        h.merge = Some(Merge::AwaitMember { peer: user, deadline: Instant::now() + s.ctx.net.timings.merge_timeout });
    }
    Next::Stay
}

/// A frame on my probe link (I am the prober).
fn probe_frame(s: &mut Session, ctl: Ctl) -> Next {
    let Role::Host(h) = &mut s.role else { return Next::Stay };
    match ctl {
        Ctl::HostCompare { claim, connected, score } => {
            let Some(Merge::Probing { peer, sent, .. }) = &h.merge else { return Next::Stay };
            let (peer, sent) = (*peer, sent.clone());
            if claim.verify_for(&s.ctx.server).is_err() || claim.claim.host != peer {
                h.merge = None;
                return Next::Stay;
            }
            let theirs = HostSide { host: peer, epoch: claim.claim.epoch, connected, score };
            s.ctx.epoch_seen = s.ctx.epoch_seen.max(claim.claim.epoch);
            if election::merge_winner(&sent, &theirs) {
                let Some(Merge::Probing { link, .. }) = h.merge.take() else { return Next::Stay };
                if let Some((new_claim, addr)) = win_merge(s, theirs.epoch) {
                    let _ = link.send(&Ctl::HostMoved { claim: new_claim, addr });
                }
                if let Role::Host(h) = &mut s.role {
                    h.retiring.push(link);
                }
            } else {
                let Some(Merge::Probing { link, peer, .. }) = h.merge.take() else { return Next::Stay };
                h.merge = Some(Merge::AwaitProbe { link, peer, deadline: Instant::now() + s.ctx.net.timings.merge_timeout });
            }
            Next::Stay
        }
        Ctl::HostMoved { claim, addr } => {
            let Some(Merge::AwaitProbe { peer, .. }) = &h.merge else { return Next::Stay };
            let valid = claim.verify_for(&s.ctx.server).is_ok() && claim.claim.host == *peer && claim.claim.epoch > h.claim.claim.epoch;
            if !valid {
                return Next::Stay;
            }
            // Join the winner over a fresh connection (normal Hello, Welcome and sync).
            let winner = claim.claim.host;
            let failed_epoch = h.claim.claim.epoch;
            s.ctx.epoch_seen = s.ctx.epoch_seen.max(claim.claim.epoch);
            s.ctx.net.learn_hint(&winner, &addr);
            lose_merge_links(s, &claim, &addr);
            Next::Seek(
                SeekKind::Failover { failed_host: s.ctx.me, failed_epoch, succession: vec![winner], rank: None },
                Some(winner),
            )
        }
        Ctl::SyncHave { heads } => {
            if let Some(Merge::Probing { link, .. } | Merge::AwaitProbe { link, .. }) = &h.merge {
                s.ctx.spawn_sync(link, heads);
            }
            Next::Stay
        }
        Ctl::Events { events, .. } => {
            let applied = s.ctx.ingest_signed(events);
            h.fanout(&s.ctx, &applied, None);
            Next::Stay
        }
        _ => Next::Stay,
    }
}

pub(super) fn on_timer(s: &mut Session, now: Instant) -> Next {
    let t = s.ctx.net.timings.clone();
    let Role::Host(h) = &mut s.role else { return Next::Stay };
    if h.handoff.as_ref().is_some_and(|ho| ho.deadline <= now) {
        return finish_handoff(s);
    }
    if h.merge.as_ref().is_some_and(|m| m.deadline() <= now) {
        tracing::debug!("merge timed out");
        if let Some(m) = h.merge.take() {
            match m {
                Merge::Probing { link, .. } | Merge::AwaitProbe { link, .. } => link.close(tincan_core::CLOSE_NORMAL, "merge timeout"),
                Merge::AwaitMember { .. } => {}
            }
        }
    }
    if h.roster_at.is_some_and(|at| at <= now) {
        h.roster_at = None;
        let roster = h.build_roster(&s.ctx, true);
        h.broadcast(&s.ctx, &Ctl::Roster(roster.clone()), None);
        s.ctx.note_roster(&roster);
        s.ctx.shared.rdv_kick.notify_one();
    }
    let Role::Host(h) = &mut s.role else { return Next::Stay };
    if s.ctx.tick_at.is_some_and(|at| at <= now) {
        if h.links.is_empty() {
            s.ctx.tick_at = None;
        } else {
            s.ctx.tick_at = Some(now + t.tick);
            s.ctx.ticks += 1;
            let dead: Vec<UserId> = h.links.iter().filter(|(_, m)| now.duration_since(m.last_rx) >= t.host_dead).map(|(u, _)| *u).collect();
            for u in dead {
                h.drop_member(&s.ctx, u, "silent");
            }
            h.broadcast(&s.ctx, &Ctl::Tick { epoch: h.claim.claim.epoch, t_ms: now_ms() }, None);
            let mut changed = false;
            for m in h.links.values_mut() {
                let p = path_snapshot(&m.link.conn);
                let bucket = |x: Option<u32>| x.map(|v| v / 25);
                if p.direct != m.path.direct || bucket(p.rtt_ms) != bucket(m.path.rtt_ms) || p.remote != m.path.remote {
                    changed = true;
                }
                m.path = p;
            }
            if changed {
                h.mark_roster(&s.ctx);
            }
            if now.duration_since(h.last_vv) >= t.vv_digest {
                h.last_vv = now;
                let digest = s.ctx.heads().digest();
                h.broadcast(&s.ctx, &Ctl::VvDigest { digest }, None);
            }
            if now.duration_since(s.ctx.last_status_at) >= t.status_every {
                s.ctx.last_status_at = now;
                let ac = power::ac_power();
                if ac != s.ctx.last_ac {
                    s.ctx.last_ac = ac;
                    h.mark_roster(&s.ctx);
                }
            }
            if now.duration_since(h.last_proactive) >= t.proactive_check && h.handoff.is_none() && h.merge.is_none() {
                h.last_proactive = now;
                let roster = h.build_roster(&s.ctx, false);
                let voice_active = roster.online.iter().any(|e| e.voice.is_some());
                let cands = h.candidates();
                let best = roster.succession.first().and_then(|u| cands.iter().find(|c| c.user == *u)).map(election::score);
                let current = h.my_score(&s.ctx);
                if h.proactive.evaluate(now, voice_active, best, current, t.proactive_hold, t.proactive_interval) {
                    tracing::info!(server = %s.ctx.server.short(), "a more stable member is online: migrating");
                    return h.start_handoff(&mut s.ctx, AfterHandoff::Member);
                }
            }
        }
    }
    let Role::Host(h) = &mut s.role else { return Next::Stay };
    if h.probe_at <= now {
        let mut cands: Vec<UserId> = Vec::new();
        if let Ok(peers) = s.ctx.net.storage.peers(&s.ctx.server, super::COLD_OPEN_PEERS) {
            for p in peers {
                s.ctx.net.learn_hint(&p.user, &p.addr);
                cands.push(p.user);
            }
        }
        if let Some(r) = &s.ctx.last_roster {
            cands.extend(r.succession.iter().copied());
        }
        let mut seen = HashSet::new();
        cands.retain(|u| *u != s.ctx.me && !h.links.contains_key(u) && seen.insert(*u));
        for (i, u) in cands.into_iter().enumerate() {
            s.ctx.dial(u, super::DialPurpose::Probe, t.dial_stagger * u32::try_from(i).unwrap_or(u32::MAX));
        }
        let interval = match h.alone_since {
            Some(since) if now.duration_since(since) >= t.solo_backoff_after => t.solo_retry_max,
            Some(_) => t.solo_retry,
            None => t.host_probe,
        };
        h.probe_at = now + jitter(interval);
    }
    Next::Stay
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proactive_needs_hold_and_interval() {
        let t0 = Instant::now();
        let hold = Duration::from_secs(600);
        let interval = Duration::from_secs(1800);
        let mut p = Proactive::new(t0);
        // Not better enough.
        assert!(!p.evaluate(t0, false, Some(110), 100, hold, interval));
        // Better, but the hold has not passed.
        assert!(!p.evaluate(t0 + Duration::from_secs(1), false, Some(200), 100, hold, interval));
        // Hold passed, interval not.
        assert!(!p.evaluate(t0 + Duration::from_secs(700), false, Some(200), 100, hold, interval));
        // Both passed.
        assert!(p.evaluate(t0 + Duration::from_secs(1900), false, Some(200), 100, hold, interval));
        // Voice resets the hold.
        assert!(!p.evaluate(t0 + Duration::from_secs(1901), true, Some(200), 100, hold, interval));
        assert!(!p.evaluate(t0 + Duration::from_secs(1902), false, Some(200), 100, hold, interval));
        // Dropping below the margin resets too.
        let mut q = Proactive::new(t0);
        assert!(!q.evaluate(t0, false, Some(200), 100, hold, interval));
        assert!(!q.evaluate(t0 + Duration::from_secs(300), false, Some(120), 100, hold, interval));
        assert!(!q.evaluate(t0 + Duration::from_secs(2000), false, Some(200), 100, hold, interval));
        assert!(q.evaluate(t0 + Duration::from_secs(2700), false, Some(200), 100, hold, interval));
    }

    #[test]
    fn pages_split_large_batches() {
        let id = tincan_core::keys::Identity::generate();
        let ev = tincan_core::build::event(
            &id,
            tincan_core::ConvId([1; 32]),
            1,
            1,
            Body::Message { channel: EventId([2; 32]), text: "x".repeat(3000), attachments: vec![] },
        )
        .unwrap();
        let many: Vec<SignedEvent> = (0..200).map(|_| ev.clone()).collect();
        let p = pages(many.iter());
        assert!(p.len() >= 2);
        let total: usize = p.iter().map(|c| if let Ctl::Events { events, .. } = c { events.len() } else { 0 }).sum();
        assert_eq!(total, 200);
    }
}
