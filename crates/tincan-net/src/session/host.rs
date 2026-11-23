//! Host role: accept members, validate and fan out events, roster, liveness, holders,
//! clean handoff, proactive migration and partition merge.
//!
//! Members are not trusted (any one may run a modified client): everything a member sends is
//! bounded per member (token buckets keyed by user id, so reconnecting does not reset them),
//! relayed chatter never takes more than half of another member's queue, and the partition
//! merge only counts members that signed an attestation (`Ctl::Attest`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use tincan_core::build::{self, JoinRejection};
use tincan_core::election::{self, Candidate, HostSide, SignedAttestation, SignedHostClaim, Status};
use tincan_core::event::{Body, SignedEvent, StoredEvent};
use tincan_core::media::{KIND_SCREEN, track_id};
use tincan_core::state::{Change, Verdict};
use tincan_core::sync::{Heads, SYNC_PAGE_BYTES};
use tincan_core::wire::{self, AddrHint, ClipInfo, Ctl, HeadsAssembler, RejectCode, Roster, RosterEntry};
use tincan_core::{BlobHash, ChannelId, EventId, UserId, now_ms};
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::{Ctx, MemberStart, Next, Role, SeekKind, Session, short_id_for};
use crate::addr::{PathSnapshot, path_snapshot};
use crate::error::{NetError, Result};
use crate::events::{ConnState, ConnectionInfo, NetEvent, PathKind};
use crate::hooks::{CtlSource, is_extra};
use crate::limits::{Bucket, Buckets};
use crate::link::{LIVE_QUEUE, Link, LinkEvent, encode};
use crate::media::HostMedia;
use crate::power;

/// Joiners (JoinAccepted, waiting for their Hello) held at once.
pub(super) const MAX_JOINERS: usize = 16;
/// `SyncHave`s per link and minute above which the link is closed (honest: a few).
const MAX_SYNCHAVE_PER_MIN: usize = 30;
/// Typing notices relayed per member and channel: one per this.
const TYPING_EVERY: Duration = Duration::from_secs(2);
/// Publishes per member: one per this, with `PUBLISH_BURST` in a row.
const PUBLISH_EVERY: Duration = Duration::from_millis(200);
const PUBLISH_BURST: u32 = 20;
/// Full history syncs per member: one per this, `SYNC_BURST` in a row (more are coalesced
/// and delayed, never dropped).
const SYNC_EVERY: Duration = Duration::from_secs(10);
const SYNC_BURST: u32 = 3;
/// WhoHas queries per member.
const WHOHAS_EVERY: Duration = Duration::from_millis(50);
const WHOHAS_BURST: u32 = 20;
/// Joins per invite (the joiner proves it holds the invite): one per 10 s, 10 in a row (an
/// invite posted in a group chat is used by several people within seconds).
const ADMIT_INVITE_EVERY: Duration = Duration::from_secs(10);
const ADMIT_INVITE_BURST: u32 = 10;
/// Admissions per responsible member (the creator of the invite a joiner used, or the author
/// of a member's own `MemberAdmit`): one per minute, 20 in a row. Keyed by member, so one
/// member cannot use up anyone else's.
const ADMIT_BY_EVERY: Duration = Duration::from_secs(60);
const ADMIT_BY_BURST: u32 = 20;
/// Merge comparisons over all peers: burst 4, then one per 15 s.
const COMPARE_EVERY: Duration = Duration::from_secs(15);
const COMPARE_BURST: u32 = 4;
/// Extra comparisons with one peer after a round that did not merge (the two histories keep
/// syncing on the retired probe link, so the next round may agree).
const MAX_REPROBES: u32 = 3;
/// How long a probe link stays open after its comparison, so both histories finish syncing.
const RETIRED_PROBE_FOR: Duration = Duration::from_secs(30);
/// Most retired probe links held at once (the oldest is closed first).
const MAX_RETIRED_PROBES: usize = 8;
/// Roster broadcasts in the last minute above which the minimum interval grows to 5 s.
const ROSTER_BUSY_PER_MIN: usize = 20;
const ROSTER_BUSY_INTERVAL: Duration = Duration::from_secs(5);
/// A member's attestation is verified at most this often (per epoch of mine).
const ATTEST_MIN_GAP: Duration = Duration::from_secs(5);

/// One link's history sync state: at most one sync in flight, the latest request queued.
#[derive(Debug, Default)]
pub(crate) struct SyncGate {
    running: bool,
    queued: Option<Heads>,
    recent: VecDeque<Instant>,
    parts: HeadsAssembler,
}

impl SyncGate {
    /// Note a request at `now`. False if the peer is flooding (close it).
    fn note(&mut self, now: Instant) -> bool {
        while self.recent.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60)) {
            self.recent.pop_front();
        }
        self.recent.push_back(now);
        self.recent.len() <= MAX_SYNCHAVE_PER_MIN
    }
}

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
    /// Complete blobs held (clip hashes included; bounded by `MAX_HAVE`).
    pub have: HashSet<BlobHash>,
    /// Soundboard library (validated, bounded by `MAX_CLIPS`).
    pub clips: Vec<ClipInfo>,
    /// Path to the member.
    pub path: PathSnapshot,
    /// Last frame from the member.
    pub last_rx: Instant,
    /// Last time the host replied to a SyncHave with its own.
    pub last_sync_reply: Option<Instant>,
    /// The member's latest valid attestation for my current epoch.
    pub attest: Option<SignedAttestation>,
    /// When it arrived (host clock: freshness never depends on the member's clock).
    attest_rx: Option<Instant>,
    /// Last attestation signature check (time, my epoch then): at most one per
    /// `ATTEST_MIN_GAP` and epoch, valid or not.
    attest_try: Option<(Instant, u64)>,
    /// History sync to this member.
    sync: SyncGate,
    /// Own admissions of this member were held back (over budget): ask it to resend then.
    resync_at: Option<Instant>,
}

/// A probe link after its comparison. Kept (with Ticks) for `RETIRED_PROBE_FOR`, so both
/// histories and membership views keep flowing even when the merge did not happen.
pub(super) struct RetiredProbe {
    link: Link,
    synced: bool,
    parts: HeadsAssembler,
    until: Instant,
    next_tick: Instant,
}

/// Partition merge in progress.
pub(super) enum Merge {
    /// I dialed another host and sent HostCompare2; waiting for its reply.
    Probing {
        link: Link,
        peer: UserId,
        sent: HostSide,
        deadline: Instant,
        synced: bool,
    },
    /// I lost as the prober; waiting for the winner's HostMoved on the probe link.
    /// `theirs_verified` is re-checked against my members when it arrives.
    AwaitProbe { link: Link, peer: UserId, deadline: Instant, synced: bool, theirs_verified: u32 },
    /// I lost to a host that probed me; waiting for its HostMoved on its member link.
    AwaitMember { peer: UserId, deadline: Instant, theirs_verified: u32 },
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
    /// The other host on my probe link.
    fn probe_peer(&self) -> Option<UserId> {
        match self {
            Merge::Probing { peer, .. } | Merge::AwaitProbe { peer, .. } => Some(*peer),
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

/// Per-member budgets, keyed by user id (they survive reconnects).
struct Gates {
    typing: Buckets<(UserId, ChannelId)>,
    publish: Buckets<UserId>,
    sync: Buckets<UserId>,
    whohas: Buckets<UserId>,
    /// Joins per invite (JoinRequest only: the joiner proved it holds the invite).
    admit_invite: Buckets<[u8; 32]>,
    /// Admissions per responsible member (see `ADMIT_BY_EVERY`).
    admit_by: Buckets<UserId>,
    compare: Bucket,
    /// Last comparison handled per peer.
    compare_last: HashMap<UserId, Instant>,
    /// Extra comparisons already scheduled per peer (at most `MAX_REPROBES`).
    reprobes: HashMap<UserId, u32>,
}

impl Gates {
    fn new(now: Instant) -> Self {
        Self {
            typing: Buckets::new(TYPING_EVERY, 1),
            publish: Buckets::new(PUBLISH_EVERY, PUBLISH_BURST),
            sync: Buckets::new(SYNC_EVERY, SYNC_BURST),
            whohas: Buckets::new(WHOHAS_EVERY, WHOHAS_BURST),
            admit_invite: Buckets::new(ADMIT_INVITE_EVERY, ADMIT_INVITE_BURST),
            admit_by: Buckets::new(ADMIT_BY_EVERY, ADMIT_BY_BURST),
            compare: Bucket::every(COMPARE_EVERY, COMPARE_BURST, now),
            compare_last: HashMap::new(),
            reprobes: HashMap::new(),
        }
    }

    /// Budget for one join through `invite_pk`, an invite created by `creator`.
    fn admit_join(&mut self, creator: &UserId, invite_pk: &[u8; 32], now: Instant) -> bool {
        // Both or neither: a refused join spends nothing.
        self.admit_by.ready(creator, now) && self.admit_invite.take(invite_pk, now) && self.admit_by.take(creator, now)
    }
}

/// State while hosting.
pub(crate) struct HostState {
    /// My current claim.
    pub claim: SignedHostClaim,
    /// Connected members.
    pub links: HashMap<UserId, MLink>,
    /// Joiners that got JoinAccepted and must send Hello next (by link id), with the
    /// deadline for that Hello.
    pub joiners: HashMap<u64, (Link, Instant)>,
    /// Links kept only until the peer closes them.
    pub retiring: Vec<Link>,
    /// Probe links after their comparison (history keeps syncing on them).
    pub(super) retired_probes: Vec<RetiredProbe>,
    /// Short ids, inherited and assigned.
    pub short_ids: HashMap<UserId, u16>,
    rev: u32,
    roster_at: Option<Instant>,
    /// Last roster broadcast (rev ignored when comparing) and when.
    roster_sent: Option<(Roster, Instant)>,
    roster_times: VecDeque<Instant>,
    since_ms: u64,
    alone_since: Option<Instant>,
    probe_at: Instant,
    pub(super) merge: Option<Merge>,
    pub(super) handoff: Option<Handoff>,
    last_vv: Instant,
    last_proactive: Instant,
    proactive: Proactive,
    gates: Gates,
    /// `SyncHavePart`s on the probe link.
    probe_parts: HeadsAssembler,
    /// SFU and rendezvous publisher of this host role (torn down when the state drops).
    _media: HostMedia,
}

/// Is a relayed frame something that may be skipped for a member that is behind (chatter
/// from other members, refreshed state)? Session frames (events, ticks, acks) never are.
fn droppable(ctl: &Ctl) -> bool { matches!(ctl, Ctl::Typing { .. } | Ctl::Roster(_) | Ctl::VvDigest { .. } | Ctl::SoundPlayed { .. }) }

/// Apply a `Have` delta within the bounds (`MAX_HAVE_DELTA` per frame, `MAX_HAVE` in total).
/// False (and nothing changed) if the delta is over the per-frame bound.
pub(super) fn apply_have(have: &mut HashSet<BlobHash>, add: Vec<BlobHash>, remove: Vec<BlobHash>) -> bool {
    if add.len() > wire::MAX_HAVE_DELTA || remove.len() > wire::MAX_HAVE_DELTA {
        return false;
    }
    for r in remove {
        have.remove(&r);
    }
    for a in add {
        if have.len() >= wire::MAX_HAVE {
            break;
        }
        have.insert(a);
    }
    true
}

/// A member's announced clips, validated and bounded.
pub(super) fn valid_clips(clips: Vec<ClipInfo>) -> Vec<ClipInfo> {
    let mut seen = HashSet::new();
    clips.into_iter().filter(|c| c.is_valid() && seen.insert(c.hash)).take(wire::MAX_CLIPS).collect()
}

impl HostState {
    /// Fresh host state (no members yet).
    pub(super) fn new(ctx: &Ctx, claim: SignedHostClaim, mut short_ids: HashMap<UserId, u16>) -> Self {
        let my_short = short_id_for(&mut short_ids, ctx.me, |_| false).unwrap_or(1);
        let rdv = ctx.keys().and_then(|k| crate::rdv::spawn_host_runner(&ctx.net, ctx.server, k, ctx.shared.clone()));
        let media = ctx.net.media.host_started(&ctx.net, ctx.server, claim.claim.epoch, my_short, rdv);
        let now = Instant::now();
        Self {
            claim,
            links: HashMap::new(),
            joiners: HashMap::new(),
            retiring: Vec::new(),
            retired_probes: Vec::new(),
            short_ids,
            rev: 0,
            roster_at: None,
            roster_sent: None,
            roster_times: VecDeque::new(),
            since_ms: now_ms(),
            alone_since: Some(now),
            probe_at: now + jitter(ctx.net.timings.solo_retry),
            merge: None,
            handoff: None,
            last_vv: now,
            last_proactive: now,
            proactive: Proactive::new(now),
            gates: Gates::new(now),
            probe_parts: HeadsAssembler::default(),
            _media: media,
        }
    }

    /// Every link this state holds.
    pub(super) fn into_links(self) -> Vec<Link> {
        let mut out: Vec<Link> = self.links.into_values().map(|m| m.link).collect();
        out.extend(self.joiners.into_values().map(|(l, _)| l));
        out.extend(self.retiring);
        out.extend(self.retired_probes.into_iter().map(|r| r.link));
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
        v.extend(self.joiners.values().map(|(_, d)| *d).min());
        v.extend(self.retired_probes.iter().map(|r| r.next_tick.min(r.until)).min());
        v.extend(self.links.values().filter_map(|m| m.resync_at).min());
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

    /// Schedule a roster broadcast: debounced, and at least `roster_min_interval` after the
    /// previous one (5 s once there were many in the last minute), so status flapping costs
    /// the host one roster per interval at most.
    pub(super) fn mark_roster(&mut self, ctx: &Ctx) {
        if self.roster_at.is_some() {
            return;
        }
        let now = Instant::now();
        let t = &ctx.net.timings;
        let min = if self.roster_times.len() > ROSTER_BUSY_PER_MIN { ROSTER_BUSY_INTERVAL.max(t.roster_min_interval) } else { t.roster_min_interval };
        let mut at = now + t.roster_debounce;
        if let Some((_, last)) = &self.roster_sent {
            at = at.max(*last + min);
        }
        self.roster_at = Some(at);
    }

    /// Send a frame to every member (except one). Members whose queue is full are dropped
    /// (their link is closed; they reconnect and resync). Relayed chatter (`droppable`) is
    /// only queued for members whose queue is less than half full and never closes anyone.
    pub(super) fn broadcast(&self, ctx: &Ctx, ctl: &Ctl, except: Option<UserId>) {
        let Some(bytes) = encode(ctl) else { return };
        let skip_when_busy = droppable(ctl);
        for (u, m) in &self.links {
            if Some(*u) == except {
                continue;
            }
            if skip_when_busy {
                if m.link.live_free() > LIVE_QUEUE / 2 {
                    let _ = m.link.send_bytes(bytes.clone());
                }
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
        if requester.is_some() && ctx.net.holds_for(&ctx.server, hash) {
            out.push((ctx.me, ctx.net.my_hint()));
        }
        for (u, m) in &self.links {
            if Some(*u) == requester {
                continue;
            }
            if m.have.contains(hash) {
                out.push((*u, m.path.remote.clone()));
                if out.len() >= wire::MAX_HOLDERS {
                    break;
                }
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

    /// Valid attestations of my connected members for my current epoch (the peer excluded),
    /// received within `ATTEST_KEEP_MS` (my clock, not the member's).
    fn my_attests(&self, exclude: Option<UserId>) -> Vec<SignedAttestation> {
        let keep = Duration::from_millis(election::ATTEST_KEEP_MS);
        let epoch = self.claim.claim.epoch;
        self.links
            .iter()
            .filter(|(u, _)| Some(**u) != exclude)
            .filter(|(_, m)| m.attest_rx.is_some_and(|t| t.elapsed() <= keep))
            .filter_map(|(_, m)| m.attest.clone())
            .filter(|a| a.att.epoch == epoch)
            .take(election::MAX_ATTESTS)
            .collect()
    }

    /// Connected members, the peer excluded.
    fn links_except(&self, exclude: Option<UserId>) -> u32 {
        u32::try_from(self.links.keys().filter(|u| Some(**u) != exclude).count()).unwrap_or(u32::MAX)
    }

    /// The winner's HostMoved arrived: may I still hand my members over? Re-checked with the
    /// members connected now (more may have joined since the comparison). A claim that is not
    /// above mine is followed only when I have no members (they could not follow it).
    fn may_follow(&self, peer: UserId, theirs_verified: u32, claim: &SignedHostClaim) -> bool {
        let links = self.links_except(Some(peer));
        election::may_yield(links, theirs_verified) && (links == 0 || claim.claim.epoch > self.claim.claim.epoch)
    }

    /// My side of a partition-merge comparison: my attested member count, the peer excluded.
    fn my_side(&self, ctx: &Ctx, exclude: Option<UserId>) -> (HostSide, Vec<SignedAttestation>) {
        let attests = self.my_attests(exclude);
        let connected = u32::try_from(attests.len()).unwrap_or(u32::MAX);
        (HostSide { host: ctx.me, epoch: self.claim.claim.epoch, connected, score: 0 }, attests)
    }

    /// How many members `peer` proved for its `claim`: attestations for exactly that claim,
    /// from current members that are neither me, the peer, nor connected to me.
    fn verified_for(&self, ctx: &Ctx, peer: UserId, claim: &SignedHostClaim, attests: &[SignedAttestation]) -> u32 {
        let r = ctx.replica.lock();
        election::count_attests(attests, &ctx.server, &peer, claim.claim.epoch, now_ms(), election::ATTEST_ACCEPT_MS, |m| {
            *m != ctx.me && *m != peer && !self.links.contains_key(m) && r.state.is_member(m)
        })
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
                // Who hears the room is what the SFU forwards to (VoiceJoin/VoiceLeave), not
                // what the member claims in its Status.
                voice: ctx.net.media.voice_of(&ctx.server, &m.link.peer),
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
        let online: HashSet<UserId> = self.links.keys().copied().chain(std::iter::once(ctx.me)).collect();
        let Some(short_id) = short_id_for(&mut self.short_ids, peer, |u| online.contains(u)) else {
            tracing::warn!(server = %ctx.server.short(), member = %peer.short(), "no short id left; refusing a member");
            link.close(tincan_core::CLOSE_REJECTED, "server full");
            return;
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
        let clips = valid_clips(clips);
        let mut have_set = HashSet::new();
        for h in have.into_iter().chain(clips.iter().map(|c| c.hash)) {
            if have_set.len() >= wire::MAX_HAVE {
                break;
            }
            have_set.insert(h);
        }
        self.links.insert(
            peer,
            MLink {
                link,
                short_id,
                since_ms: now_ms(),
                status,
                status_at: now,
                have: have_set,
                clips,
                path,
                last_rx: now,
                // The Welcome below is followed by my SyncHave already.
                last_sync_reply: Some(now),
                attest: None,
                attest_rx: None,
                attest_try: None,
                sync: SyncGate::default(),
                resync_at: None,
            },
        );
        let roster = self.build_roster(ctx, false);
        let next_seq = welcome_next_seq(ctx, ctx.replica.lock().state.next_seq(&peer));
        let heads = ctx.heads();
        if let Some(m) = self.links.get(&peer) {
            let _ = m.link.send(&Ctl::Welcome { claim: self.claim.clone(), roster, your_short_id: short_id, your_next_seq: next_seq });
            ctx.send_heads(&m.link, heads);
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

    /// Start a history sync to member `user` unless one runs (then the request is queued) or
    /// its budget is spent (then it waits for the timer). Never dropped: a member's first
    /// sync is what lets it publish.
    fn request_sync(&mut self, ctx: &Ctx, user: UserId, heads: Heads, now: Instant) {
        let Some(m) = self.links.get_mut(&user) else { return };
        if m.sync.running || !self.gates.sync.take(&user, now) {
            m.sync.queued = Some(heads);
            return;
        }
        m.sync.running = true;
        ctx.spawn_sync(&m.link, heads);
    }

    /// A sync finished on `link`, or the timer fired: start queued syncs that may run now.
    fn pump_syncs(&mut self, ctx: &Ctx, now: Instant) {
        let users: Vec<UserId> = self.links.iter().filter(|(_, m)| !m.sync.running && m.sync.queued.is_some()).map(|(u, _)| *u).collect();
        for u in users {
            if let Some(heads) = self.links.get_mut(&u).and_then(|m| m.sync.queued.take()) {
                self.request_sync(ctx, u, heads, now);
            }
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
        // Admissions are permanent for every replica: bound how fast one invite, and the
        // invites of one member, admit (keyed by its creator, so a member flooding with its
        // own invites cannot hold up joins through anyone else's).
        let invite_pk = match ctl {
            Ctl::JoinRequest { invite_pk, .. } => *invite_pk,
            _ => [0; 32],
        };
        let busy = || JoinRejection { code: RejectCode::Busy, reason: "too many people are joining right now; try again in a minute".into() };
        if verified.already_member {
            // A rejoin costs a scan of the admissions: from the member's own budget.
            if !self.gates.admit_by.take(&verified.joiner, Instant::now()) {
                return Err(busy());
            }
        } else {
            let creator = ctx.replica.lock().state.invites.get(&invite_pk).map(|i| i.creator);
            let creator = creator.ok_or_else(|| JoinRejection { code: RejectCode::BadInvite, reason: "unknown invite".into() })?;
            if !self.gates.admit_join(&creator, &invite_pk, Instant::now()) {
                return Err(busy());
            }
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

    /// Events from `sender`, with the sender's own admissions of new members cut to its
    /// admission budget (an admission is permanent for every replica). Only events the sender
    /// authored are looked at: relayed history is never budgeted or held back, so one member
    /// cannot hold up anyone else's history. A budgeted admission must carry a valid
    /// signature and be the sender's next seq (one that would wait in the pending buffer
    /// would later apply unbudgeted). Over budget, it and the sender's later events in the
    /// batch are held back; the second value says so (ask the sender to resend later).
    fn budget_admits(&mut self, ctx: &Ctx, sender: UserId, events: Vec<SignedEvent>) -> (Vec<SignedEvent>, bool) {
        let now = Instant::now();
        let r = ctx.replica.lock();
        let mut next = r.state.head(&sender).saturating_add(1);
        let mut held = false;
        let mut out = Vec::with_capacity(events.len());
        for s in events {
            let Ok(ev) = s.decode() else {
                out.push(s);
                continue;
            };
            if ev.author != sender {
                out.push(s);
                continue;
            }
            if held {
                continue;
            }
            let new_admit = matches!(&ev.body, Body::MemberAdmit { user, .. } if !r.state.is_member(user));
            if ev.seq != next {
                if new_admit && ev.seq > next {
                    held = true;
                    continue;
                }
                out.push(s);
                continue;
            }
            next = next.saturating_add(1);
            // A bad signature is not charged; ingesting it drops the sender.
            if new_admit && s.verify().is_ok() && !self.gates.admit_by.take(&sender, now) {
                held = true;
                continue;
            }
            out.push(s);
        }
        (out, held)
    }

    /// May a comparison with `peer` be looked at now? At most once per `compare_cooldown`
    /// per peer, counted before the signature is checked (a garbage compare only spends the
    /// sender's own turn). The shared budget is taken after the signature check
    /// (`compare_budget`).
    fn compare_peer_ok(&mut self, ctx: &Ctx, peer: UserId, now: Instant) -> bool {
        let cooldown = ctx.net.timings.compare_cooldown;
        if self.gates.compare_last.get(&peer).is_some_and(|t| now.duration_since(*t) < cooldown) {
            return false;
        }
        if self.gates.compare_last.len() > 1024 {
            self.gates.compare_last.retain(|_, t| now.duration_since(*t) < cooldown);
        }
        self.gates.compare_last.insert(peer, now);
        true
    }

    /// The shared comparison budget (over all peers), for a compare whose claim verified.
    fn compare_budget(&mut self, now: Instant) -> bool { self.gates.compare.take(now) }

    /// Keep a probe link open after its comparison so both histories finish syncing.
    fn retire_probe(&mut self, link: Link, synced: bool, parts: HeadsAssembler, tick: Duration) {
        let now = Instant::now();
        if self.retired_probes.len() >= MAX_RETIRED_PROBES {
            let old = self.retired_probes.remove(0);
            old.link.close(tincan_core::CLOSE_NORMAL, "probe done");
        }
        self.retired_probes.push(RetiredProbe { link, synced, parts, until: now + RETIRED_PROBE_FOR, next_tick: now + tick });
    }
}

/// `your_next_seq` for a Welcome (test hook: a malicious host's bogus value).
fn welcome_next_seq(ctx: &Ctx, honest: u64) -> u64 {
    #[cfg(test)]
    {
        let bogus = ctx.net.test.welcome_next_seq.load(std::sync::atomic::Ordering::Relaxed);
        if bogus != 0 {
            return bogus;
        }
    }
    let _ = ctx;
    honest
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
    let (sent, attests) = h.my_side(&s.ctx, Some(peer));
    tracing::info!(server = %s.ctx.server.short(), other = %peer.short(), "found another host: comparing");
    let _ = link.send(&Ctl::HostCompare2 { claim: h.claim.clone(), attests });
    s.ctx.send_heads(&link, s.ctx.heads());
    h.merge = Some(Merge::Probing { link, peer, sent, deadline: Instant::now() + s.ctx.net.timings.merge_timeout, synced: false });
    Next::Stay
}

/// The epoch a merge win claims. `Ok(None)`: keep my claim, because it is already above the
/// loser's, or because the loser proved no members (a loser with no members follows a claim
/// that is not above its own; one with unproven members stays, see `may_follow`), so a
/// member cannot make the host bump its epoch with a bare compare. `Ok(Some(e))`: claim `e`,
/// one above the loser, so the loser's members can follow. `Err(())`: that would be above
/// what my members accept now (the loser's epoch is at the ceiling): not now.
fn win_epoch(mine: u64, theirs: u64, theirs_verified: u32) -> std::result::Result<Option<u64>, ()> {
    if mine > theirs || theirs_verified == 0 {
        return Ok(None);
    }
    match election::next_epoch(theirs) {
        Some(e) if election::epoch_claimable(e, now_ms()) => Ok(Some(e)),
        _ => Err(()),
    }
}

/// I won a merge. With `bump == None` my current claim is enough (no new claim, no record
/// write, no broadcast). Otherwise claim `bump` and refresh my members. Returns the claim to
/// send to the loser.
fn win_merge(s: &mut Session, bump: Option<u64>) -> Option<(SignedHostClaim, AddrHint)> {
    let Role::Host(h) = &mut s.role else { return None };
    let addr = s.ctx.net.my_hint();
    let Some(epoch) = bump else {
        tracing::info!(server = %s.ctx.server.short(), epoch = h.claim.claim.epoch, "won the merge");
        return Some((h.claim.clone(), addr));
    };
    let claim = match build::host_claim(&s.ctx.net.identity, &s.ctx.server, epoch, Some(s.ctx.me), now_ms()) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "cannot sign host claim");
            return None;
        }
    };
    h.claim = claim.clone();
    s.ctx.epoch_seen = s.ctx.epoch_seen.max(epoch);
    s.ctx.update_record(epoch, s.ctx.me);
    // My own members just refresh their claim (same host, new epoch) and re-attest.
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
        LinkEvent::SyncSent { link } => {
            if let Some(user) = h.user_of(link)
                && let Some(m) = h.links.get_mut(&user)
            {
                m.sync.running = false;
                h.pump_syncs(&s.ctx, Instant::now());
            }
            Next::Stay
        }
        LinkEvent::Closed { link, .. } => {
            if h.joiners.remove(&link).is_some() {
                return Next::Stay;
            }
            if let Some(pos) = h.retiring.iter().position(|l| l.id == link) {
                h.retiring.swap_remove(pos);
                return Next::Stay;
            }
            if let Some(pos) = h.retired_probes.iter().position(|r| r.link.id == link) {
                h.retired_probes.swap_remove(pos);
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
        LinkEvent::Frame { link, ctl, .. } => {
            let ctl = *ctl;
            if let Some((j, _)) = h.joiners.remove(&link) {
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
            if h.retired_probes.iter().any(|r| r.link.id == link) {
                return retired_probe_frame(s, link, ctl);
            }
            match h.user_of(link) {
                Some(user) => member_frame(s, user, ctl),
                None => Next::Stay,
            }
        }
    }
}

/// A member's (complete) SyncHave.
fn member_sync_have(s: &mut Session, user: UserId, heads: Heads, now: Instant) {
    let Role::Host(h) = &mut s.role else { return };
    let Some(m) = h.links.get_mut(&user) else { return };
    if !m.sync.note(now) {
        tracing::info!(server = %s.ctx.server.short(), member = %user.short(), "SyncHave flood; dropping the member");
        h.drop_member(&s.ctx, user, "sync flood");
        return;
    }
    let mine = s.ctx.heads();
    let interval = s.ctx.net.timings.vv_digest;
    let reply = !heads.missing_in(&mine).is_empty() && m.last_sync_reply.is_none_or(|t| t.elapsed() >= interval / 2);
    if reply {
        m.last_sync_reply = Some(now);
    }
    let link_reply = reply.then(|| mine.clone());
    h.request_sync(&s.ctx, user, heads, now);
    if let Some(mine) = link_reply
        && let Some(m) = h.links.get(&user)
    {
        s.ctx.send_heads(&m.link, mine);
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
            let id = event.id();
            let (ack, applied) = if !h.gates.publish.take(&user, now) {
                // Retryable: the member re-sends the same signed event (its seq is kept).
                (Ctl::PublishAck { id, ok: false, reason: "busy".into() }, Vec::new())
            } else if publish_refusal(&s.ctx).is_some() {
                let (_, applied) = member_publish(&mut s.ctx, &mut h.gates, user, event);
                (Ctl::PublishAck { id, ok: false, reason: publish_refusal(&s.ctx).unwrap_or_default() }, applied)
            } else {
                member_publish(&mut s.ctx, &mut h.gates, user, event)
            };
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
            let (events, held) = h.budget_admits(&s.ctx, user, events);
            if held
                && let Some(m) = h.links.get_mut(&user)
                && m.resync_at.is_none()
            {
                // Its own admissions wait for its budget: ask it to resend then.
                m.resync_at = Some(now + ADMIT_BY_EVERY);
            }
            let (applied, bad) = s.ctx.ingest_checked(events);
            let Role::Host(h) = &mut s.role else { return Next::Stay };
            h.fanout(&s.ctx, &applied, Some(user));
            if bad {
                // Honest members only forward events they verified.
                h.drop_member(&s.ctx, user, "sent an event with a bad signature");
            }
            Next::Stay
        }
        Ctl::SyncHave { heads } => {
            member_sync_have(s, user, heads, now);
            Next::Stay
        }
        Ctl::SyncHavePart { heads, more } => {
            if let Some(full) = m.sync.parts.push(heads, more) {
                member_sync_have(s, user, full, now);
            }
            Next::Stay
        }
        Ctl::Status(st) => {
            // Applied at once; the roster that carries it is rate limited (`mark_roster`).
            m.status = st;
            m.status_at = now;
            h.mark_roster(&s.ctx);
            Next::Stay
        }
        Ctl::Tick { .. } => Next::Stay,
        Ctl::Have { add, remove } => {
            if !apply_have(&mut m.have, add, remove) {
                tracing::debug!(member = %user.short(), "Have delta over the bounds ignored");
            }
            // Clip hashes stay held while announced.
            for c in &m.clips {
                m.have.insert(c.hash);
            }
            Next::Stay
        }
        Ctl::WhoHas { hash } => {
            if !h.gates.whohas.take(&user, now) {
                return Next::Stay;
            }
            let holders = h.holders(&s.ctx, &hash, Some(user));
            if let Some(m) = h.links.get(&user) {
                let _ = m.link.send(&Ctl::Holders { hash, holders });
            }
            Next::Stay
        }
        Ctl::Typing { channel, by } => {
            // Only for a text channel the member may post in: the budget's keys stay bounded
            // by members x channels.
            let may = by == user && s.ctx.replica.lock().state.can_post(&user, &channel);
            if may && h.gates.typing.take(&(user, channel), now) {
                h.broadcast(&s.ctx, &Ctl::Typing { channel, by }, Some(user));
                s.ctx.net.emit(NetEvent::Typing { conv: s.ctx.server, channel, user });
            }
            Next::Stay
        }
        Ctl::Attest(a) => {
            let epoch = h.claim.claim.epoch;
            let shape = a.att.server == s.ctx.server
                && a.att.member == user
                && a.att.host == s.ctx.me
                && a.att.epoch == epoch
                && a.fresh(now_ms(), election::ATTEST_ACCEPT_MS);
            // At most one signature check per `ATTEST_MIN_GAP` and epoch of mine, counted
            // before verifying (valid or not).
            let due = m.attest_try.is_none_or(|(t, e)| e != epoch || now.duration_since(t) >= ATTEST_MIN_GAP);
            if !shape || !due {
                return Next::Stay;
            }
            m.attest_try = Some((now, epoch));
            if a.verify().is_err() {
                // Honest members sign their own attestations.
                h.drop_member(&s.ctx, user, "sent an attestation with a bad signature");
                return Next::Stay;
            }
            m.attest = Some(a);
            m.attest_rx = Some(now);
            Next::Stay
        }
        Ctl::HostCompare { claim, .. } => compare_from_member(s, user, claim, Vec::new()),
        Ctl::HostCompare2 { claim, attests } => compare_from_member(s, user, claim, attests),
        Ctl::HostMoved { claim, addr } => {
            let valid = claim.verify_for(&s.ctx.server).is_ok() && claim.claim.host == user && s.ctx.epoch_ok(claim.claim.epoch);
            if !valid {
                return Next::Stay;
            }
            if let Some(ho) = &mut h.handoff
                && ho.target == user
            {
                if claim.claim.epoch > h.claim.claim.epoch {
                    ho.new_claim = Some((claim, addr));
                }
                return Next::Stay;
            }
            if let Some(Merge::AwaitMember { peer, theirs_verified, .. }) = &h.merge
                && *peer == user
            {
                let theirs_verified = *theirs_verified;
                if !h.may_follow(user, theirs_verified, &claim) {
                    tracing::info!(server = %s.ctx.server.short(), other = %user.short(), "merge: my members changed meanwhile; staying");
                    h.merge = None;
                    return Next::Stay;
                }
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

/// Test hook: a malicious host refuses every publish after storing it.
fn publish_refusal(ctx: &Ctx) -> Option<String> {
    #[cfg(test)]
    {
        if let Some(r) = ctx.net.test.refuse_publish.lock().clone() {
            return Some(r);
        }
    }
    let _ = ctx;
    None
}

/// Validate a member's Publish; returns the ack and the applied events.
fn member_publish(ctx: &mut Ctx, gates: &mut Gates, from: UserId, signed: SignedEvent) -> (Ctl, Vec<(StoredEvent, Change)>) {
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
            // A member's own admission of someone takes its own admission budget (any member
            // may admit with any invite, so the invite's budget is for joins only).
            if matches!(&stored.event.body, Body::MemberAdmit { .. }) && !gates.admit_by.take(&from, Instant::now()) {
                return (ack(false, "busy"), Vec::new());
            }
            let res = {
                let mut r = ctx.replica.lock();
                r.ingest(ctx.net.storage.as_ref(), &ctx.net.hlc, stored)
            };
            ctx.after_applied(&res.applied);
            (ack(true, ""), res.applied)
        }
        Verdict::Stale => {
            let have = matches!(ctx.net.storage.event(&id), Ok(Some(_)));
            if !have {
                // Same (author, seq), different event: keep the proof and say so.
                let res = ctx.replica.lock().ingest(ctx.net.storage.as_ref(), &ctx.net.hlc, stored.clone());
                if res.conflict.is_some() {
                    ctx.net.emit(NetEvent::Equivocation { conv: ctx.server, author: from, seq: stored.event.seq });
                }
            }
            (ack(have, if have { "" } else { "conflicts with an earlier event" }), Vec::new())
        }
        Verdict::Gap | Verdict::Future => (ack(false, "gap"), Vec::new()),
        Verdict::Reject(reason) => (ack(false, &reason), Vec::new()),
    }
}

/// Another host (connected to me as a member) wants to compare.
///
/// The comparison counts only verified attestations (a peer cannot inflate its member count,
/// and the self-reported score is ignored). Even when it loses, a host only hands its members
/// over to a peer that proved at least as many members as it has connected
/// (`election::may_yield`), re-checked when the winner's HostMoved arrives (`may_follow`).
/// When both sides count the same members they agree on one winner; when their membership
/// views differ both may think they won, and nothing moves until a later round.
fn compare_from_member(s: &mut Session, user: UserId, claim: SignedHostClaim, attests: Vec<SignedAttestation>) -> Next {
    let now = Instant::now();
    let Role::Host(h) = &mut s.role else { return Next::Stay };
    if h.merge.is_some() || h.handoff.is_some() || claim.claim.host != user || attests.len() > election::MAX_ATTESTS {
        return Next::Stay;
    }
    // The sender's own turn first (before any signature check), the shared budget only for a
    // claim that verified.
    if !h.compare_peer_ok(&s.ctx, user, now) {
        return Next::Stay;
    }
    if claim.verify_for(&s.ctx.server).is_err() || !s.ctx.epoch_ok(claim.claim.epoch) || !h.compare_budget(now) {
        return Next::Stay;
    }
    let theirs_verified = h.verified_for(&s.ctx, user, &claim, &attests);
    let theirs = HostSide { host: user, epoch: claim.claim.epoch, connected: theirs_verified, score: 0 };
    let (mine, my_attests) = h.my_side(&s.ctx, Some(user));
    let wins = election::merge_winner_attested(&mine, &theirs);
    let bump = if wins {
        match win_epoch(h.claim.claim.epoch, theirs.epoch, theirs_verified) {
            Ok(b) => b,
            Err(()) => {
                tracing::info!(server = %s.ctx.server.short(), other = %user.short(), "merge: cannot claim above the other host's epoch yet");
                return Next::Stay;
            }
        }
    } else {
        None
    };
    if let Some(m) = h.links.get(&user) {
        let _ = m.link.send(&Ctl::HostCompare2 { claim: h.claim.clone(), attests: my_attests });
    }
    if wins {
        if let Some((claim, addr)) = win_merge(s, bump)
            && bump.is_none()
            && let Role::Host(h) = &s.role
            && let Some(m) = h.links.get(&user)
        {
            // No new claim: only the loser needs mine (a new claim was broadcast to all).
            let _ = m.link.send(&Ctl::HostMoved { claim, addr });
        }
    } else if election::may_yield(h.links_except(Some(user)), theirs_verified) {
        h.merge = Some(Merge::AwaitMember { peer: user, deadline: now + s.ctx.net.timings.merge_timeout, theirs_verified });
    } else {
        tracing::info!(
            server = %s.ctx.server.short(), other = %user.short(), theirs_verified,
            "merge: the other host proved fewer members than I have; not moving mine"
        );
    }
    Next::Stay
}

/// A frame on my probe link (I am the prober).
fn probe_frame(s: &mut Session, ctl: Ctl) -> Next {
    let Role::Host(h) = &mut s.role else { return Next::Stay };
    match ctl {
        Ctl::HostCompare { claim, .. } => probe_compare(s, claim, Vec::new()),
        Ctl::HostCompare2 { claim, attests } => probe_compare(s, claim, attests),
        Ctl::HostMoved { claim, addr } => {
            let Some(Merge::AwaitProbe { peer, theirs_verified, .. }) = &h.merge else { return Next::Stay };
            let (peer, theirs_verified) = (*peer, *theirs_verified);
            let valid = claim.verify_for(&s.ctx.server).is_ok() && claim.claim.host == peer && s.ctx.epoch_ok(claim.claim.epoch);
            if !valid {
                return Next::Stay;
            }
            if !h.may_follow(peer, theirs_verified, &claim) {
                tracing::info!(server = %s.ctx.server.short(), other = %peer.short(), "merge: my members changed meanwhile; staying");
                end_probe(s, peer);
                return Next::Stay;
            }
            // Join the winner over a fresh connection (normal Hello, Welcome and sync).
            let failed_epoch = h.claim.claim.epoch;
            s.ctx.epoch_seen = s.ctx.epoch_seen.max(claim.claim.epoch);
            s.ctx.net.learn_hint(&peer, &addr);
            lose_merge_links(s, &claim, &addr);
            Next::Seek(SeekKind::Failover { failed_host: s.ctx.me, failed_epoch, succession: vec![peer], rank: None }, Some(peer))
        }
        Ctl::SyncHave { heads } => {
            probe_sync(s, heads);
            Next::Stay
        }
        Ctl::SyncHavePart { heads, more } => {
            if let Some(full) = h.probe_parts.push(heads, more) {
                probe_sync(s, full);
            }
            Next::Stay
        }
        Ctl::Events { events, .. } => {
            let Some(peer) = h.merge.as_ref().and_then(Merge::probe_peer) else { return Next::Stay };
            let (events, _) = h.budget_admits(&s.ctx, peer, events);
            let (applied, bad) = s.ctx.ingest_checked(events);
            let Role::Host(h) = &mut s.role else { return Next::Stay };
            h.fanout(&s.ctx, &applied, None);
            if bad && let Some(m) = h.merge.take() {
                match m {
                    Merge::Probing { link, .. } | Merge::AwaitProbe { link, .. } => link.close(tincan_core::CLOSE_NORMAL, "bad event"),
                    Merge::AwaitMember { .. } => {}
                }
            }
            Next::Stay
        }
        _ => Next::Stay,
    }
}

/// The probed host's SyncHave on my probe link: one history sync per probe link.
fn probe_sync(s: &mut Session, heads: Heads) {
    let Role::Host(h) = &mut s.role else { return };
    if let Some(Merge::Probing { link, synced, .. } | Merge::AwaitProbe { link, synced, .. }) = &mut h.merge
        && !*synced
    {
        *synced = true;
        s.ctx.spawn_sync(link, heads);
    }
}

/// A frame on a retired probe link: only history (both ways) and membership flow on.
fn retired_probe_frame(s: &mut Session, link: u64, ctl: Ctl) -> Next {
    let Role::Host(h) = &mut s.role else { return Next::Stay };
    let Some(r) = h.retired_probes.iter_mut().find(|r| r.link.id == link) else { return Next::Stay };
    let peer = r.link.peer;
    match ctl {
        Ctl::SyncHave { heads } => {
            if !r.synced {
                r.synced = true;
                s.ctx.spawn_sync(&r.link, heads);
            }
        }
        Ctl::SyncHavePart { heads, more } => {
            if let Some(full) = r.parts.push(heads, more)
                && !r.synced
            {
                r.synced = true;
                s.ctx.spawn_sync(&r.link, full);
            }
        }
        Ctl::Events { events, .. } => {
            let (events, _) = h.budget_admits(&s.ctx, peer, events);
            let (applied, bad) = s.ctx.ingest_checked(events);
            let Role::Host(h) = &mut s.role else { return Next::Stay };
            h.fanout(&s.ctx, &applied, None);
            if bad && let Some(pos) = h.retired_probes.iter().position(|r| r.link.id == link) {
                h.retired_probes.swap_remove(pos).link.close(tincan_core::CLOSE_NORMAL, "bad event");
            }
        }
        _ => {}
    }
    Next::Stay
}

/// The probed host's reply to my HostCompare2.
fn probe_compare(s: &mut Session, claim: SignedHostClaim, attests: Vec<SignedAttestation>) -> Next {
    let Role::Host(h) = &mut s.role else { return Next::Stay };
    let Some(Merge::Probing { peer, sent, .. }) = &h.merge else { return Next::Stay };
    let (peer, sent) = (*peer, sent.clone());
    let valid = claim.verify_for(&s.ctx.server).is_ok()
        && claim.claim.host == peer
        && s.ctx.epoch_ok(claim.claim.epoch)
        && attests.len() <= election::MAX_ATTESTS;
    if !valid {
        if let Some(Merge::Probing { link, .. }) = h.merge.take() {
            link.close(tincan_core::CLOSE_NORMAL, "bad compare");
        }
        return Next::Stay;
    }
    let theirs_verified = h.verified_for(&s.ctx, peer, &claim, &attests);
    let theirs = HostSide { host: peer, epoch: claim.claim.epoch, connected: theirs_verified, score: 0 };
    if election::merge_winner_attested(&sent, &theirs) {
        let Ok(bump) = win_epoch(h.claim.claim.epoch, theirs.epoch, theirs_verified) else {
            tracing::info!(server = %s.ctx.server.short(), other = %peer.short(), "merge: cannot claim above the other host's epoch yet");
            end_probe(s, peer);
            return Next::Stay;
        };
        let Some(Merge::Probing { link, synced, .. }) = h.merge.take() else { return Next::Stay };
        if let Some((new_claim, addr)) = win_merge(s, bump) {
            let _ = link.send(&Ctl::HostMoved { claim: new_claim, addr });
        }
        let tick = s.ctx.net.timings.tick;
        if let Role::Host(h) = &mut s.role {
            let parts = std::mem::take(&mut h.probe_parts);
            h.retire_probe(link, synced, parts, tick);
        }
        // A loser that counted my members differently stays; compare again once the
        // histories synced (skipped if it joined me meanwhile).
        reprobe(s, peer);
    } else if election::may_yield(h.links_except(Some(peer)), theirs_verified) {
        let Some(Merge::Probing { link, peer, synced, .. }) = h.merge.take() else { return Next::Stay };
        h.merge = Some(Merge::AwaitProbe { link, peer, deadline: Instant::now() + s.ctx.net.timings.merge_timeout, synced, theirs_verified });
    } else {
        tracing::info!(server = %s.ctx.server.short(), other = %peer.short(), theirs_verified, "merge: not moving my members");
        end_probe(s, peer);
    }
    Next::Stay
}

/// My probe round ended without a merge: keep its link syncing for a while and compare again
/// later.
fn end_probe(s: &mut Session, peer: UserId) {
    let tick = s.ctx.net.timings.tick;
    let Role::Host(h) = &mut s.role else { return };
    if let Some(Merge::Probing { link, synced, .. } | Merge::AwaitProbe { link, synced, .. }) = h.merge.take() {
        let parts = std::mem::take(&mut h.probe_parts);
        h.retire_probe(link, synced, parts, tick);
    }
    reprobe(s, peer);
}

/// Compare with `peer` once more after the cooldown (at most `MAX_REPROBES` times per host
/// role): the retired probe link keeps both histories syncing, so membership views that
/// differed agree by then.
fn reprobe(s: &mut Session, peer: UserId) {
    let Role::Host(h) = &mut s.role else { return };
    let n = h.gates.reprobes.entry(peer).or_default();
    if *n >= MAX_REPROBES {
        return;
    }
    *n += 1;
    let cooldown = s.ctx.net.timings.compare_cooldown;
    s.ctx.schedule_probe(peer, cooldown + cooldown / 4);
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
                Merge::Probing { link, .. } => link.close(tincan_core::CLOSE_NORMAL, "merge timeout"),
                // The other host answered: keep the histories flowing.
                Merge::AwaitProbe { link, synced, .. } => {
                    let parts = std::mem::take(&mut h.probe_parts);
                    h.retire_probe(link, synced, parts, t.tick);
                }
                Merge::AwaitMember { .. } => {}
            }
        }
    }
    // Retired probe links: Ticks keep the other host from dropping them while the histories
    // sync; closed when their time is up.
    let epoch = h.claim.claim.epoch;
    h.retired_probes.retain_mut(|r| {
        if r.until <= now {
            r.link.close(tincan_core::CLOSE_NORMAL, "probe done");
            return false;
        }
        if r.next_tick <= now {
            r.next_tick = now + t.tick;
            let _ = r.link.send(&Ctl::Tick { epoch, t_ms: now_ms() });
        }
        true
    });
    // Members whose own admissions waited for their budget: ask them to resend.
    let due: Vec<UserId> = h.links.iter().filter(|(_, m)| m.resync_at.is_some_and(|at| at <= now)).map(|(u, _)| *u).collect();
    if !due.is_empty() {
        let heads = s.ctx.heads();
        for u in due {
            if let Some(m) = h.links.get_mut(&u) {
                m.resync_at = None;
                s.ctx.send_heads(&m.link, heads.clone());
            }
        }
    }
    // Joiners that never sent their Hello.
    let late: Vec<u64> = h.joiners.iter().filter(|(_, (_, d))| *d <= now).map(|(id, _)| *id).collect();
    for id in late {
        if let Some((l, _)) = h.joiners.remove(&id) {
            l.close(tincan_core::CLOSE_REJECTED, "no hello");
        }
    }
    if h.roster_at.is_some_and(|at| at <= now) {
        h.roster_at = None;
        let roster = h.build_roster(&s.ctx, false);
        let same = h.roster_sent.as_ref().is_some_and(|(r, _)| Roster { rev: r.rev, ..roster.clone() } == *r);
        if !same {
            let roster = h.build_roster(&s.ctx, true);
            h.broadcast(&s.ctx, &Ctl::Roster(roster.clone()), None);
            while h.roster_times.front().is_some_and(|x| now.duration_since(*x) >= Duration::from_secs(60)) {
                h.roster_times.pop_front();
            }
            h.roster_times.push_back(now);
            h.roster_sent = Some((roster.clone(), now));
            s.ctx.note_roster(&roster);
            s.ctx.shared.rdv_kick.notify_one();
        }
    }
    let Role::Host(h) = &mut s.role else { return Next::Stay };
    h.pump_syncs(&s.ctx, now);
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

    #[test]
    fn have_and_clips_are_bounded() {
        let mut have = HashSet::new();
        // A delta over the per-frame bound changes nothing.
        let big: Vec<BlobHash> = (0..=wire::MAX_HAVE_DELTA as u32).map(|i| BlobHash(*blake3::hash(&i.to_le_bytes()).as_bytes())).collect();
        assert!(!apply_have(&mut have, big, vec![]));
        assert!(have.is_empty());
        // Many deltas stop at MAX_HAVE.
        for chunk in 0..((wire::MAX_HAVE / wire::MAX_HAVE_DELTA) as u32 + 3) {
            let add: Vec<BlobHash> =
                (0..wire::MAX_HAVE_DELTA as u32).map(|i| BlobHash(*blake3::hash(&(chunk * 100_000 + i).to_le_bytes()).as_bytes())).collect();
            assert!(apply_have(&mut have, add, vec![]));
        }
        assert_eq!(have.len(), wire::MAX_HAVE);
        // Clips: invalid ones dropped, duplicates dropped, at most MAX_CLIPS kept.
        let clip = |i: u32, name: &str, size: u64| ClipInfo { hash: BlobHash(*blake3::hash(&i.to_le_bytes()).as_bytes()), name: name.into(), size, duration_ms: 500 };
        let mut clips: Vec<ClipInfo> = (0..1000).map(|i| clip(i, "ok", 10)).collect();
        clips.push(clip(5000, &"n".repeat(500), 10));
        clips.push(clip(5001, "big", u64::MAX));
        clips.push(clip(1, "dup", 10));
        let kept = valid_clips(clips);
        assert_eq!(kept.len(), wire::MAX_CLIPS);
        assert!(kept.iter().all(|c| c.is_valid()));
    }

    #[test]
    fn joins_are_rate_limited_per_invite_and_per_creator() {
        let t0 = Instant::now();
        let (owner, mallory) = (UserId([1; 32]), UserId([2; 32]));
        let (inv, other) = ([1u8; 32], [2u8; 32]);
        // A burst per invite, then one per 10 s.
        let mut g = Gates::new(t0);
        let burst = (0..30).filter(|_| g.admit_join(&owner, &inv, t0)).count();
        assert_eq!(burst, ADMIT_INVITE_BURST as usize);
        assert!(g.admit_join(&owner, &other, t0), "another invite has its own budget");
        assert!(g.admit_join(&owner, &inv, t0 + ADMIT_INVITE_EVERY));
        // One member's invites share that member's budget, and a refused join spends nothing.
        let mut h = Gates::new(t0);
        let many = (0..200u32).filter(|i| h.admit_join(&mallory, &[*i as u8; 32], t0)).count();
        assert_eq!(many, ADMIT_BY_BURST as usize);
        assert!(!h.admit_join(&mallory, &[250; 32], t0));
        // ... but not anyone else's: the owner's invite still admits.
        assert!(h.admit_join(&owner, &inv, t0), "a flooding member cannot hold up other members' invites");
        let mut k = Gates::new(t0);
        assert_eq!((0..ADMIT_INVITE_BURST).filter(|_| k.admit_join(&owner, &inv, t0)).count(), ADMIT_INVITE_BURST as usize);
        assert!(!k.admit_join(&mallory, &inv, t0), "this invite is spent");
        assert!(k.admit_join(&mallory, &other, t0), "the refusal did not spend Mallory's budget");
    }

    #[test]
    fn relayed_chatter_is_droppable_session_frames_are_not() {
        assert!(droppable(&Ctl::Typing { channel: EventId([1; 32]), by: UserId([2; 32]) }));
        assert!(droppable(&Ctl::Roster(Roster::default())));
        assert!(!droppable(&Ctl::Events { events: vec![], more: false }));
        assert!(!droppable(&Ctl::Tick { epoch: 1, t_ms: 1 }));
    }
}
