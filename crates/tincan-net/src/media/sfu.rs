//! The host SFU of one server: exists while this device hosts it.
//!
//! - Audio: each member's datagrams are read by one task and sent verbatim to the other
//!   members of the sender's voice room (and to the host's own app when it is in that room).
//!   A datagram whose track id is not the sender's own voice track is dropped. The host's own
//!   packets are injected the same way.
//! - Video: each member's GOP uploads are put in order and pushed into that track's
//!   [`VideoTrack`], whose viewers are the members (and the host's app) that subscribed.
//! - Every second while anything is active: AIMD targets go to sharers (`TargetBitrate`),
//!   the worst receiver report per voice track goes to its owner (`ReceiverReport`, which the
//!   owner turns into `NetEvent::LossReport`).
//!
//! Lock order: the SFU lock may be held while a track's lock is taken, never the other way;
//! track methods that can call the keyframe callback (which takes the SFU lock) are only
//! called after the SFU lock is released.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::Bytes;
use iroh::endpoint::Connection;
use parking_lot::Mutex;
use tincan_core::event::ChannelKind;
use tincan_core::media::{AudioHeader, KIND_SCREEN, KIND_SOUND, KIND_VOICE, VideoFrame, track_id, track_kind, track_short_id};
use tincan_core::wire::{ClipInfo, Ctl};
use tincan_core::{BlobHash, ChannelId, ServerId, UserId};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use super::recv::{Pause, VideoSink, spawn_video_rx};
use super::stats::{Report, TrackRx};
use super::track::{VideoTrack, ViewerId};
use super::wire::{CtlTx, VideoOut};
use super::{HOST_UPLINK_BUDGET_BPS, RELAYED_SHARER_BPS, RELAYED_VIEWER_BPS, VIDEO_MAX_BPS};
use crate::events::NetEvent;
use crate::inner::Inner;
use crate::limits::{AudioPolicer, VideoPolicer, VideoVerdict};
use crate::link::Tasks;

/// Media tick (bitrate targets, receiver reports).
pub(crate) const MEDIA_TICK: Duration = Duration::from_secs(1);
/// Datagrams taken per read.
const DATAGRAM_BATCH: usize = 32;

/// The host's own app in one of the server's rooms.
pub(crate) struct LocalPeer {
    /// Its voice room.
    pub channel: ChannelId,
    /// Where room audio goes.
    pub audio_in: mpsc::Sender<Bytes>,
    /// Where subscribed video goes.
    pub video_in: VideoOut,
    /// Subscribed tracks.
    pub subs: HashSet<u16>,
}

struct Member {
    short_id: u16,
    conn: Connection,
    ctl: CtlTx,
    voice: Option<ChannelId>,
    relayed: bool,
    subs: HashSet<u16>,
    tasks: Vec<AbortHandle>,
}

impl Member {
    fn abort(&self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Everyone in one voice room (forwarding snapshot, rebuilt on change).
struct Room {
    members: Vec<(UserId, Connection, CtlTx)>,
    local: Option<mpsc::Sender<Bytes>>,
}

struct State {
    my_short: u16,
    members: HashMap<UserId, Member>,
    local: Option<LocalPeer>,
    local_track: Option<u16>,
    tracks: HashMap<u16, Arc<VideoTrack>>,
    rooms: HashMap<ChannelId, Arc<Room>>,
    uplink: HashMap<u16, TrackRx>,
    reports: HashMap<u16, Report>,
    last_loss: Option<u8>,
    ticker: Option<AbortHandle>,
    closed: bool,
    /// Audio datagram policers per member (kept across a member's reconnects).
    policers: HashMap<UserId, AudioPolicer>,
    /// Last soundboard announcement relayed per member.
    sound_last: HashMap<UserId, Instant>,
    /// Clips announced recently per member (relayed once per `SOUND_DEDUPE`).
    sound_seen: HashMap<(UserId, BlobHash), Instant>,
}

/// A member's soundboard announcements are relayed at most this often.
const SOUND_EVERY: Duration = Duration::from_secs(2);
/// The same clip is announced at most once in this window.
const SOUND_DEDUPE: Duration = Duration::from_secs(30);

impl State {
    fn room(&mut self, ch: ChannelId) -> Arc<Room> {
        if let Some(r) = self.rooms.get(&ch) {
            return r.clone();
        }
        let members = self.members.iter().filter(|(_, m)| m.voice == Some(ch)).map(|(u, m)| (*u, m.conn.clone(), m.ctl.clone())).collect();
        let local = self.local.as_ref().filter(|l| l.channel == ch).map(|l| l.audio_in.clone());
        let r = Arc::new(Room { members, local });
        self.rooms.insert(ch, r.clone());
        r
    }

    fn member_by_short(&self, short: u16) -> Option<&Member> { self.members.values().find(|m| m.short_id == short) }

    fn active(&self) -> bool {
        self.local.is_some() || self.members.values().any(|m| m.voice.is_some()) || self.tracks.values().any(|t| !t.viewer_ids().is_empty())
    }
}

/// Keep the report with the highest loss (then jitter) per track.
fn merge_worst(reports: &mut HashMap<u16, Report>, track: u16, r: Report) {
    let e = reports.entry(track).or_insert(r);
    if (r.loss_pct(), r.jitter_ms) > (e.loss_pct(), e.jitter_ms) {
        *e = r;
    }
}

/// The SFU of one hosted server.
pub(crate) struct Sfu {
    /// The server.
    pub server: ServerId,
    net: Weak<Inner>,
    tasks: Arc<Tasks>,
    pause: Pause,
    st: Mutex<State>,
}

impl std::fmt::Debug for Sfu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "Sfu({})", self.server.short()) }
}

impl Sfu {
    /// An empty SFU (no members yet). `my_short` is the host's own short id.
    pub fn new(net: &Arc<Inner>, server: ServerId, my_short: u16) -> Arc<Self> {
        Arc::new(Self {
            server,
            net: Arc::downgrade(net),
            tasks: net.tasks.clone(),
            pause: net.media.pause(),
            st: Mutex::new(State {
                my_short,
                members: HashMap::new(),
                local: None,
                local_track: None,
                tracks: HashMap::new(),
                rooms: HashMap::new(),
                uplink: HashMap::new(),
                reports: HashMap::new(),
                last_loss: None,
                ticker: None,
                closed: false,
                policers: HashMap::new(),
                sound_last: HashMap::new(),
                sound_seen: HashMap::new(),
            }),
        })
    }

    /// The voice room the SFU forwards to `user` (set only by VoiceJoin / VoiceLeave).
    pub fn voice_of(&self, user: &UserId) -> Option<ChannelId> { self.st.lock().members.get(user).and_then(|m| m.voice) }

    /// A track (tests).
    #[cfg(test)]
    pub fn track(&self, id: u16) -> Option<Arc<VideoTrack>> { self.st.lock().tracks.get(&id).cloned() }

    /// A member's link came up (or was replaced).
    pub fn add_member(self: &Arc<Self>, user: UserId, short_id: u16, conn: Connection, ctl: CtlTx) {
        let weak = Arc::downgrade(self);
        let fwd = self.tasks.spawn(forward_task(weak.clone(), conn.clone(), user, short_id));
        let upload = Upload { sfu: weak, short_id, policer: Mutex::new(VideoPolicer::new(Instant::now())) };
        let vid = spawn_video_rx(&self.tasks, conn.clone(), Arc::new(upload), self.pause.clone());
        let relayed = crate::addr::path_snapshot(&conn).relay;
        let member = Member { short_id, conn, ctl, voice: None, relayed, subs: HashSet::new(), tasks: vec![fwd, vid] };
        let old = {
            let mut st = self.st.lock();
            if st.closed {
                member.abort();
                return;
            }
            st.rooms.clear();
            st.members.insert(user, member)
        };
        if let Some(old) = old {
            old.abort();
            self.forget_viewer(user);
        }
    }

    /// A member's link went away. `stable` is that link's `Connection::stable_id`, so a late
    /// notice about a replaced link does not remove its replacement.
    pub fn remove_member(&self, user: UserId, stable: usize) {
        let (m, owned) = {
            let mut st = self.st.lock();
            if st.members.get(&user).is_none_or(|m| m.conn.stable_id() != stable) {
                return;
            }
            let Some(m) = st.members.remove(&user) else { return };
            st.rooms.clear();
            let local_track = st.local_track;
            let owned: Vec<Arc<VideoTrack>> = {
                let ids: Vec<u16> =
                    st.tracks.keys().copied().filter(|t| track_short_id(*t) == m.short_id && Some(*t) != local_track).collect();
                ids.into_iter().filter_map(|t| st.tracks.remove(&t)).collect()
            };
            for kind in [KIND_VOICE, KIND_SOUND] {
                let t = track_id(m.short_id, kind);
                st.uplink.remove(&t);
                st.reports.remove(&t);
            }
            (m, owned)
        };
        m.abort();
        for t in owned {
            t.end();
            t.clear();
        }
        self.forget_viewer(user);
    }

    fn forget_viewer(&self, user: UserId) {
        let tracks: Vec<Arc<VideoTrack>> = self.st.lock().tracks.values().cloned().collect();
        for t in tracks {
            t.remove(ViewerId::Peer(user));
        }
    }

    /// The host role ended: stop everything.
    pub fn shutdown(&self) {
        let (members, tracks, ticker) = {
            let mut st = self.st.lock();
            st.closed = true;
            st.local = None;
            st.local_track = None;
            st.rooms.clear();
            let members: Vec<Member> = st.members.drain().map(|(_, m)| m).collect();
            let tracks: Vec<Arc<VideoTrack>> = st.tracks.drain().map(|(_, t)| t).collect();
            (members, tracks, st.ticker.take())
        };
        for m in members {
            m.abort();
        }
        for t in tracks {
            t.clear();
        }
        if let Some(t) = ticker {
            t.abort();
        }
    }

    /// The host's app joined a room of this server.
    pub fn set_local(self: &Arc<Self>, local: LocalPeer) {
        let (subs, sink) = (local.subs.clone(), local.video_in.clone());
        let tracks: Vec<Arc<VideoTrack>> = {
            let mut st = self.st.lock();
            if st.closed {
                return;
            }
            st.local = Some(local);
            st.rooms.clear();
            self.ensure_ticker(&mut st);
            subs.iter().filter_map(|t| st.tracks.get(t).cloned()).collect()
        };
        for t in tracks {
            t.add_local(sink.clone());
        }
    }

    /// The host's app left its room.
    pub fn clear_local(&self) {
        let tracks: Vec<Arc<VideoTrack>> = {
            let mut st = self.st.lock();
            st.local = None;
            st.rooms.clear();
            st.tracks.values().cloned().collect()
        };
        for t in tracks {
            t.remove(ViewerId::Local);
        }
    }

    /// The host's app watches `track`.
    pub fn local_subscribe(self: &Arc<Self>, track: u16) {
        let found = {
            let mut st = self.st.lock();
            let Some(l) = st.local.as_mut() else { return };
            l.subs.insert(track);
            let sink = l.video_in.clone();
            self.ensure_ticker(&mut st);
            st.tracks.get(&track).cloned().map(|t| (t, sink))
        };
        if let Some((t, sink)) = found {
            t.add_local(sink);
        }
    }

    /// The host's app stops watching `track`.
    pub fn local_unsubscribe(&self, track: u16) {
        let t = {
            let mut st = self.st.lock();
            if let Some(l) = st.local.as_mut() {
                l.subs.remove(&track);
            }
            st.tracks.get(&track).cloned()
        };
        if let Some(t) = t {
            t.remove(ViewerId::Local);
        }
    }

    /// The host's own share: members that subscribed to it become its viewers.
    pub fn attach_local_track(self: &Arc<Self>, track: Arc<VideoTrack>) {
        let id = track.track;
        let (remotes, local, old) = {
            let mut st = self.st.lock();
            if st.closed {
                return;
            }
            let remotes: Vec<(UserId, Connection)> =
                st.members.iter().filter(|(_, m)| m.subs.contains(&id)).map(|(u, m)| (*u, m.conn.clone())).collect();
            let local = st.local.as_ref().filter(|l| l.subs.contains(&id)).map(|l| l.video_in.clone());
            let old = st.local_track.replace(id).and_then(|o| if o != id { st.tracks.remove(&o) } else { None });
            st.tracks.insert(id, track.clone());
            self.ensure_ticker(&mut st);
            (remotes, local, old)
        };
        if let Some(o) = old {
            o.clear();
        }
        for (u, c) in remotes {
            track.add_remote(u, c);
        }
        if let Some(s) = local {
            track.add_local(s);
        }
    }

    /// The host's own share stopped (or moves elsewhere).
    pub fn detach_local_track(&self) {
        let t = {
            let mut st = self.st.lock();
            st.local_track.take().and_then(|id| st.tracks.remove(&id))
        };
        if let Some(t) = t {
            t.clear();
        }
    }

    /// A datagram from the host's own engine: to everyone in the host's room.
    pub fn inject_local(&self, pkt: Bytes) {
        let room = {
            let mut st = self.st.lock();
            let Some(ch) = st.local.as_ref().map(|l| l.channel) else { return };
            st.room(ch)
        };
        for (_, c, _) in &room.members {
            let _ = c.send_datagram(pkt.clone());
        }
    }

    /// The host played a soundboard clip: tell its room.
    pub fn local_sound(&self, by: UserId, clip: ClipInfo) {
        let room = {
            let mut st = self.st.lock();
            let Some(ch) = st.local.as_ref().map(|l| l.channel) else { return };
            st.room(ch)
        };
        let ctl = Ctl::SoundPlayed { by, clip };
        for (_, _, tx) in &room.members {
            let _ = tx.send(&ctl);
        }
    }

    /// Forward a batch of datagrams from `from`. Each member's datagrams are policed (size,
    /// packets and bytes per second) before anything is counted or fanned out.
    fn forward(&self, from: UserId, short_id: u16, pkts: &mut [Bytes]) {
        let now = Instant::now();
        let room = {
            let mut st = self.st.lock();
            let Some(ch) = st.members.get(&from).and_then(|m| m.voice) else { return };
            let st = &mut *st;
            let policer = st.policers.entry(from).or_insert_with(|| AudioPolicer::new(now));
            for p in pkts.iter_mut() {
                if super::wire::audio_track_from(p, short_id).is_none() || !policer.allow(p.len(), now) {
                    *p = Bytes::new();
                    continue;
                }
                if let Some((h, _)) = AudioHeader::parse(p) {
                    st.uplink.entry(h.track).or_default().audio(h.seq, h.ts, now);
                }
            }
            st.room(ch)
        };
        for p in pkts.iter_mut() {
            let p = std::mem::take(p);
            if p.is_empty() {
                continue;
            }
            for (u, c, _) in &room.members {
                if *u != from {
                    let _ = c.send_datagram(p.clone());
                }
            }
            if let Some(l) = &room.local {
                let _ = l.try_send(p);
            }
        }
    }

    /// The track an upload belongs to (created on first use, with every subscriber).
    fn upload_track(self: &Arc<Self>, track: u16) -> Option<Arc<VideoTrack>> {
        let (t, remotes, local) = {
            let mut st = self.st.lock();
            if st.closed {
                return None;
            }
            if let Some(t) = st.tracks.get(&track) {
                return Some(t.clone());
            }
            let weak = Arc::downgrade(self);
            let t = VideoTrack::new(
                track,
                self.tasks.clone(),
                Box::new(move |id| {
                    if let Some(s) = weak.upgrade() {
                        s.keyframe_upstream(id);
                    }
                }),
            );
            let remotes: Vec<(UserId, Connection)> =
                st.members.iter().filter(|(_, m)| m.subs.contains(&track)).map(|(u, m)| (*u, m.conn.clone())).collect();
            let local = st.local.as_ref().filter(|l| l.subs.contains(&track)).map(|l| l.video_in.clone());
            st.tracks.insert(track, t.clone());
            self.ensure_ticker(&mut st);
            (t, remotes, local)
        };
        for (u, c) in remotes {
            t.add_remote(u, c);
        }
        if let Some(s) = local {
            t.add_local(s);
        }
        Some(t)
    }

    /// Ask a remote sharer for a keyframe.
    fn keyframe_upstream(&self, track: u16) {
        let ctl = self.st.lock().member_by_short(track_short_id(track)).map(|m| m.ctl.clone());
        if let Some(c) = ctl {
            let _ = c.send(&Ctl::KeyframeRequest { track });
        }
    }

    /// The roster shows rooms as the SFU forwards them: tell the session to rebuild it.
    fn voice_changed(&self, net: &Inner) {
        if let Some(s) = net.sessions.lock().get(&self.server) {
            let _ = s.tx.try_send(crate::session::SessionCmd::StatusChanged);
        }
    }

    /// May `member` report on, or ask for keyframes of, `track`? Screen tracks: only a
    /// subscriber. Voice tracks: only someone in the owner's room.
    fn may_report(st: &State, member: &UserId, track: u16) -> bool {
        let Some(m) = st.members.get(member) else { return false };
        if track_kind(track) == KIND_SCREEN {
            return m.subs.contains(&track);
        }
        let owner = track_short_id(track);
        let owner_room = if owner == st.my_short { st.local.as_ref().map(|l| l.channel) } else { st.member_by_short(owner).and_then(|o| o.voice) };
        m.voice.is_some() && m.voice == owner_room
    }

    /// A media control message from a member.
    pub fn on_ctl(self: &Arc<Self>, net: &Inner, member: UserId, short_id: u16, ctl: Ctl) {
        match ctl {
            Ctl::VoiceJoin { channel } => {
                if !is_voice_channel(net, &self.server, &channel) {
                    tracing::debug!(member = %member.short(), "VoiceJoin for a channel that is not a voice room");
                    return;
                }
                let changed = {
                    let mut st = self.st.lock();
                    let changed = st.members.get_mut(&member).is_some_and(|m| m.voice.replace(channel) != Some(channel));
                    if st.members.contains_key(&member) {
                        st.rooms.clear();
                        self.ensure_ticker(&mut st);
                    }
                    changed
                };
                if changed {
                    self.voice_changed(net);
                }
            }
            Ctl::VoiceLeave => {
                let changed = {
                    let mut st = self.st.lock();
                    let changed = st.members.get_mut(&member).is_some_and(|m| m.voice.take().is_some());
                    if changed {
                        st.rooms.clear();
                        st.uplink.remove(&track_id(short_id, KIND_VOICE));
                        st.uplink.remove(&track_id(short_id, KIND_SOUND));
                    }
                    changed
                };
                if changed {
                    self.voice_changed(net);
                }
            }
            Ctl::Subscribe { track } if track_kind(track) == KIND_SCREEN => {
                let found = {
                    let mut st = self.st.lock();
                    let Some(m) = st.members.get_mut(&member) else { return };
                    // Already subscribed: nothing to do (a repeated Subscribe must not
                    // re-seed the whole GOP cache).
                    if !m.subs.insert(track) {
                        return;
                    }
                    let conn = m.conn.clone();
                    self.ensure_ticker(&mut st);
                    st.tracks.get(&track).cloned().map(|t| (t, conn))
                };
                if let Some((t, conn)) = found {
                    t.add_remote(member, conn);
                }
            }
            Ctl::Unsubscribe { track } => {
                let t = {
                    let mut st = self.st.lock();
                    if let Some(m) = st.members.get_mut(&member) {
                        m.subs.remove(&track);
                    }
                    st.tracks.get(&track).cloned()
                };
                if let Some(t) = t {
                    t.remove(ViewerId::Peer(member));
                }
            }
            Ctl::KeyframeRequest { track } => {
                let t = {
                    let st = self.st.lock();
                    if !Self::may_report(&st, &member, track) {
                        return;
                    }
                    st.tracks.get(&track).cloned()
                };
                if let Some(t) = t {
                    t.viewer_wants_key(ViewerId::Peer(member));
                }
            }
            Ctl::ReceiverReport { track, rx, lost, late, jitter_ms } => {
                let r = Report { rx, lost, late, jitter_ms };
                let mut st = self.st.lock();
                if !Self::may_report(&st, &member, track) {
                    return;
                }
                if track_kind(track) == KIND_VOICE && track_short_id(track) != short_id {
                    merge_worst(&mut st.reports, track, r);
                } else if track_kind(track) == KIND_SCREEN && lost > 0 {
                    let t = st.tracks.get(&track).cloned();
                    drop(st);
                    if let Some(t) = t {
                        t.report_loss();
                    }
                }
            }
            Ctl::SoundPlayed { by, clip } if by == member && clip.is_valid() => {
                let now = Instant::now();
                let room = {
                    let mut st = self.st.lock();
                    let Some(ch) = st.members.get(&member).and_then(|m| m.voice) else { return };
                    if st.sound_last.get(&member).is_some_and(|t| now.duration_since(*t) < SOUND_EVERY)
                        || st.sound_seen.get(&(member, clip.hash)).is_some_and(|t| now.duration_since(*t) < SOUND_DEDUPE)
                    {
                        return;
                    }
                    st.sound_last.insert(member, now);
                    st.sound_seen.retain(|_, t| now.duration_since(*t) < SOUND_DEDUPE);
                    st.sound_seen.insert((member, clip.hash), now);
                    st.room(ch)
                };
                let fwd = Ctl::SoundPlayed { by, clip: clip.clone() };
                for (u, _, tx) in &room.members {
                    if *u != member {
                        let _ = tx.send_relayed(&fwd);
                    }
                }
                if room.local.is_some() {
                    net.emit(NetEvent::SoundPlayed { conv: self.server, by, clip });
                }
            }
            other => tracing::trace!(?other, "SFU: ignoring control message"),
        }
    }

    fn ensure_ticker(self: &Arc<Self>, st: &mut State) {
        if st.ticker.is_some() || st.closed {
            return;
        }
        let weak = Arc::downgrade(self);
        st.ticker = Some(self.tasks.spawn(async move {
            let mut next = Instant::now() + MEDIA_TICK;
            loop {
                tokio::time::sleep_until(next).await;
                next += MEDIA_TICK;
                let Some(s) = weak.upgrade() else { return };
                if !s.tick() {
                    return;
                }
            }
        }));
    }

    /// One media tick. False when nothing is active any more (the ticker stops).
    fn tick(&self) -> bool {
        let now = Instant::now();
        let mut targets: Vec<(u16, u32, Option<CtlTx>)> = Vec::new();
        let mut reports_out: Vec<(CtlTx, Ctl)> = Vec::new();
        let mut loss_changed = None;
        let my_short;
        let active = {
            let mut st = self.st.lock();
            if st.closed {
                st.ticker = None;
                return false;
            }
            my_short = st.my_short;
            for m in st.members.values_mut() {
                m.relayed = crate::addr::path_snapshot(&m.conn).relay;
            }
            let viewer_lists: Vec<(u16, Arc<VideoTrack>, Vec<ViewerId>)> =
                st.tracks.iter().map(|(id, t)| (*id, t.clone(), t.viewer_ids())).collect();
            let streams: usize = viewer_lists.iter().map(|(_, _, v)| v.iter().filter(|x| matches!(x, ViewerId::Peer(_))).count()).sum();
            let per_stream = HOST_UPLINK_BUDGET_BPS / u32::try_from(streams.max(1)).unwrap_or(u32::MAX);
            for (id, t, viewers) in viewer_lists {
                let owner = st.member_by_short(track_short_id(id));
                let mut cap = VIDEO_MAX_BPS.min(per_stream);
                let relayed_viewer =
                    viewers.iter().any(|v| matches!(v, ViewerId::Peer(u) if st.members.get(u).is_some_and(|m| m.relayed)));
                if relayed_viewer {
                    cap = cap.min(RELAYED_VIEWER_BPS);
                }
                if owner.is_some_and(|m| m.relayed) {
                    cap = cap.min(RELAYED_SHARER_BPS);
                }
                if let Some(bps) = t.tick(now, cap) {
                    targets.push((id, bps, owner.map(|m| m.ctl.clone())));
                }
            }
            let mut reports = std::mem::take(&mut st.reports);
            for (t, rx) in st.uplink.iter_mut() {
                if let Some(r) = rx.take() {
                    merge_worst(&mut reports, *t, r);
                }
            }
            for (track, r) in reports {
                let owner = track_short_id(track);
                if owner == st.my_short {
                    let pct = r.loss_pct();
                    if st.last_loss != Some(pct) {
                        st.last_loss = Some(pct);
                        loss_changed = Some(pct);
                    }
                } else if let Some(m) = st.member_by_short(owner) {
                    let ctl = Ctl::ReceiverReport { track, rx: r.rx, lost: r.lost, late: r.late, jitter_ms: r.jitter_ms };
                    reports_out.push((m.ctl.clone(), ctl));
                }
            }
            let active = st.active();
            if !active {
                st.ticker = None;
            }
            active
        };
        let net = self.net.upgrade();
        for (track, bps, ctl) in targets {
            match ctl {
                Some(c) => {
                    let _ = c.send(&Ctl::TargetBitrate { track, bps });
                }
                None if track_short_id(track) == my_short => {
                    if let Some(n) = &net {
                        n.emit(NetEvent::TargetBitrate { track, bps });
                    }
                }
                None => {}
            }
        }
        for (c, ctl) in reports_out {
            let _ = c.send(&ctl);
        }
        if let (Some(pct), Some(n)) = (loss_changed, &net) {
            n.emit(NetEvent::LossReport { pct });
        }
        active
    }
}

/// Is `ch` a live voice channel of `server`?
fn is_voice_channel(net: &Inner, server: &ServerId, ch: &ChannelId) -> bool {
    net.replica(server).is_some_and(|r| r.lock().state.channels.get(ch).is_some_and(|c| c.kind == ChannelKind::Voice && !c.deleted))
}

/// Uploads from one member: only its own screen track is accepted, and its ingress is
/// policed (a sharer that ignores `TargetBitrate` cannot push more than the cap to every
/// viewer).
struct Upload {
    sfu: Weak<Sfu>,
    short_id: u16,
    policer: Mutex<VideoPolicer>,
}

impl VideoSink for Upload {
    fn accept(&self, track: u16) -> bool { track_kind(track) == KIND_SCREEN && track_short_id(track) == self.short_id }

    fn frame(&self, track: u16, f: VideoFrame) -> bool {
        let verdict = self.policer.lock().check(f.data.len(), f.keyframe, Instant::now());
        if let VideoVerdict::Drop { gap } = verdict {
            if gap {
                tracing::debug!(track, "video upload over its budget: dropping to the next keyframe");
                self.gap(track);
            }
            return true;
        }
        if let Some(t) = self.sfu.upgrade().and_then(|s| s.upload_track(track)) {
            t.push(f);
        }
        true
    }

    fn gap(&self, track: u16) {
        let t = self.sfu.upgrade().and_then(|s| s.st.lock().tracks.get(&track).cloned());
        if let Some(t) = t {
            t.gap();
        }
    }
}

/// Read one member's datagrams and forward them.
async fn forward_task(sfu: Weak<Sfu>, conn: Connection, user: UserId, short_id: u16) {
    let mut buf = vec![Bytes::new(); DATAGRAM_BATCH];
    loop {
        let n = match conn.read_many_datagrams(&mut buf).await {
            Ok(n) => n,
            Err(_) => break,
        };
        let Some(s) = sfu.upgrade() else { break };
        s.forward(user, short_id, &mut buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worst_report_wins() {
        let mut m = HashMap::new();
        merge_worst(&mut m, 4, Report { rx: 100, lost: 0, late: 0, jitter_ms: 3 });
        merge_worst(&mut m, 4, Report { rx: 90, lost: 10, late: 0, jitter_ms: 1 });
        merge_worst(&mut m, 4, Report { rx: 99, lost: 1, late: 0, jitter_ms: 50 });
        assert_eq!(m[&4].lost, 10);
        merge_worst(&mut m, 8, Report { rx: 10, lost: 0, late: 0, jitter_ms: 1 });
        merge_worst(&mut m, 8, Report { rx: 10, lost: 0, late: 0, jitter_ms: 9 });
        assert_eq!(m[&8].jitter_ms, 9);
    }
}
