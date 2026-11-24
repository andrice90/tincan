//! Voice sessions: which `MediaSession` the engine is attached to, and everything that
//! follows from it (pumps, `voice-updated`, calls, mute and deafen, per-user volume,
//! devices, the mic test).
//!
//! `Net` emits `VoiceUpdated` / `CallUpdated` before a command gets its `MediaSession`
//! back, so attach and detach are decided by [`Media::reconcile`] from the current
//! `Net::voice_info` and `Net::call_info`, never from event payloads. Commands that change
//! the session mark themselves as switching so reconcile does not detach in between.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use serde_json::Value;
use tincan_core::media::{KIND_VOICE, VideoFrame, track_id, ui_video_message};
use tincan_core::storage::Storage;
use tincan_core::{ChannelId, ConvId, ServerId, UserId};
use tincan_net::{CallEnd, CallInfo, CallState, ConnState, MediaSession, Net, VoiceInfo};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use super::{Media, State, engine};
use crate::app::{Core, net_err};
use crate::dto::{self, ToastKind};
use crate::emit::Ev;
use crate::views::path_kind;

/// Upload cap for files and clips served to peers while I am in voice (architecture.md:
/// files must not starve the call). No uplink measurement exists, so this is fixed.
pub(super) const VOICE_UPLOAD_LIMIT: u64 = 256 * 1024;
/// Upload cap while I also share my screen.
pub(super) const SHARE_UPLOAD_LIMIT: u64 = 128 * 1024;
/// kv key of the per-user volumes (JSON object: z32 -> gain).
const VOLUMES_KEY: &str = "app/user_volumes";

/// What a session is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// A server voice room.
    Room {
        /// The server.
        server: ServerId,
        /// The voice channel.
        channel: ChannelId,
    },
    /// A 1:1 call.
    Call {
        /// The other side.
        peer: UserId,
    },
}

/// The session the engine is attached to.
pub(super) struct Attached {
    /// What it is.
    pub kind: Kind,
    /// Server or DM conversation.
    pub conv: ConvId,
    /// Where the engine sends encoded audio (kept to restart a dead engine).
    pub audio_out: mpsc::Sender<Bytes>,
    /// My voice track as last reported.
    pub local_track: u16,
    /// Host epoch byte as last reported.
    pub epoch: u8,
    /// The path is relayed.
    pub relayed: bool,
    /// Audio and video pumps.
    tasks: Vec<AbortHandle>,
}

/// My ringing-out call: the engine starts when it becomes active.
pub(super) struct PendingCall {
    /// Who I called.
    pub peer: UserId,
    /// Its session (media flows once accepted).
    pub session: MediaSession,
}

/// Short ids and members of my session.
#[derive(Default, PartialEq, Eq)]
pub(super) struct RoomMap {
    /// Short id -> user (every online member of the server, or the call peer).
    pub short: HashMap<u16, UserId>,
    /// The others in my room (or the call peer).
    pub users: HashSet<UserId>,
}

impl RoomMap {
    fn short_of(&self, user: &UserId) -> Option<u16> { self.short.iter().find(|(_, u)| *u == user).map(|(s, _)| *s) }
}

/// Is the roster of a server in this state current?
fn roster_current(s: ConnState) -> bool { matches!(s, ConnState::Member | ConnState::Hosting | ConnState::Solo) }

/// A session-changing command in flight. Reconcile does not detach while one exists.
pub(super) struct Switching<'a> {
    media: &'a Media,
    armed: bool,
}

impl Switching<'_> {
    /// The command is done (with `st` locked).
    pub(super) fn finish(mut self, st: &mut State) {
        st.switching = st.switching.saturating_sub(1);
        self.armed = false;
    }
}

impl Drop for Switching<'_> {
    fn drop(&mut self) {
        // Only when the command future was dropped before `finish`.
        if self.armed {
            let mut st = self.media.st.lock();
            st.switching = st.switching.saturating_sub(1);
        }
    }
}

/// `CallInfo` DTO (None once the call ended).
pub(super) fn call_dto(core: &Core, c: &CallInfo) -> Option<dto::CallInfo> {
    let state = match c.state {
        CallState::RingingOut => dto::CallState::RingingOut,
        CallState::RingingIn => dto::CallState::RingingIn,
        CallState::Active => dto::CallState::Active,
        CallState::Ended => return None,
    };
    Some(dto::CallInfo { call_id: c.call_id.to_string(), user_id: c.peer.to_z32(), name: core.user_name(&c.peer), state })
}

/// The notice shown when a call ends, given its state before.
fn end_notice(ended: Option<CallEnd>, prev: Option<CallState>, name: &str) -> Option<(ToastKind, String)> {
    use CallState::{Active, RingingIn, RingingOut};
    match (ended?, prev?) {
        (CallEnd::Declined, RingingOut) => Some((ToastKind::Info, format!("{name} declined the call."))),
        (CallEnd::NoAnswer, RingingOut) => Some((ToastKind::Info, format!("{name} didn't answer."))),
        (CallEnd::NoAnswer | CallEnd::Cancelled, RingingIn) => Some((ToastKind::Info, format!("Missed call from {name}."))),
        (CallEnd::Lost, Active) => Some((ToastKind::Error, format!("The call with {name} was lost."))),
        (CallEnd::Lost, RingingOut | RingingIn) => Some((ToastKind::Error, format!("Couldn't reach {name}. Try again later."))),
        _ => None,
    }
}

/// Persisted per-user volumes.
pub(super) fn load_volumes(core: &Core) -> HashMap<UserId, f32> {
    let Ok(Some(raw)) = core.storage.get_kv(VOLUMES_KEY) else { return HashMap::new() };
    let map: HashMap<String, f32> = serde_json::from_slice(&raw).unwrap_or_default();
    map.into_iter().filter_map(|(k, v)| Some((UserId::from_z32(&k)?, if v.is_finite() { v.clamp(0.0, 2.0) } else { 1.0 }))).collect()
}

fn save_volumes(core: &Core, volumes: &HashMap<UserId, f32>) {
    let map: HashMap<String, f32> = volumes.iter().map(|(u, v)| (u.to_z32(), *v)).collect();
    if let Ok(json) = serde_json::to_vec(&map)
        && let Err(e) = core.storage.set_kv(VOLUMES_KEY, &json)
    {
        tracing::warn!(error = %e, "saving user volumes failed");
    }
}

/// How loud `user` plays for me (0..2), as the UI's volume slider shows it.
fn volume_of(st: &State, user: &UserId) -> f64 {
    let v = f64::from(st.volumes.get(user).copied().unwrap_or(1.0));
    (v * 1000.0).round() / 1000.0
}

/// Hang up a call whose audio could not start (Net already answered or connected it), so
/// neither side is left in a silent call that nothing in the UI can end.
fn hang_up_unattached(core: &Arc<Core>) {
    let core = core.clone();
    tokio::spawn(async move { core.net.call_hangup().await });
}

impl Media {
    /// Mark a session-changing command as in flight.
    pub(super) fn begin_switch(&self) -> Switching<'_> {
        self.st.lock().switching += 1;
        Switching { media: self, armed: true }
    }

    /// Attach the engine to `session` (detaching any other first).
    pub(super) fn attach_locked(&self, core: &Arc<Core>, st: &mut State, session: MediaSession, kind: Kind) -> Result<(), String> {
        if st.attached.is_some() {
            self.detach_locked(core, st);
        }
        let MediaSession { conv, local_track, epoch, audio_out, audio_in, video_in } = session;
        self.start_engine_call(core, st, local_track, epoch, &audio_out)?;
        if let Some(e) = self.engine_handle() {
            // A previous session may have left the relayed jitter maximum on.
            e.set_relayed(false);
        }
        let tasks = vec![self.spawn_audio_pump(audio_in), self.spawn_video_pump(core.net.clone(), video_in)];
        st.attached = Some(Attached { kind, conv, audio_out, local_track, epoch, relayed: false, tasks });
        core.files.set_upload_limit(Some(VOICE_UPLOAD_LIMIT));
        self.refresh_room(core, st);
        tracing::info!(conv = %conv.short(), call = matches!(kind, Kind::Call { .. }), "voice attached");
        Ok(())
    }

    /// Stop the engine's call side and forget everything tied to the session.
    pub(super) fn detach_locked(&self, core: &Arc<Core>, st: &mut State) {
        let Some(a) = st.attached.take() else { return };
        for t in a.tasks {
            t.abort();
        }
        if let Some(e) = self.engine_handle() {
            e.stop_call();
        }
        for user in std::mem::take(&mut st.speaking) {
            core.events.emit(Ev::Speaking, dto::Speaking { user_id: user.to_z32(), speaking: false });
        }
        self.watchers.lock().clear();
        if let Some(share) = st.share.take() {
            share.stop_in_background();
        }
        if !st.heard.is_empty() {
            st.heard.clear();
            self.emit_clips_locked(core, st);
        }
        st.sound_seen.clear();
        st.pcm.clear();
        st.room = RoomMap::default();
        core.files.set_upload_limit(None);
        tracing::info!(conv = %a.conv.short(), "voice detached");
    }

    /// Bring the attachment in line with the network's current session, then re-send
    /// `voice-updated` if it changed.
    pub(super) fn reconcile(&self, core: &Arc<Core>) {
        if self.is_closed() {
            return;
        }
        // Read the network's session only while `st` is held: a command that finished its
        // switch before we got the lock already sees its new room in `voice_info`, and one
        // that has not is still counted in `switching`. Read before the lock, a stale `None`
        // from the middle of `Net::voice_join` would detach the room it just joined.
        // (Net never calls into Media, and voice_dto already reads Net under `st`.)
        let mut st = self.st.lock();
        let info = core.net.voice_info();
        let call = core.net.call_info();
        if st.switching == 0 {
            let stale = st.attached.as_ref().is_some_and(|a| match a.kind {
                Kind::Room { server, channel } => !info.as_ref().is_some_and(|i| i.conv == server && i.channel == Some(channel)),
                Kind::Call { peer } => !call.as_ref().is_some_and(|c| c.peer == peer && c.state == CallState::Active),
            });
            if stale {
                self.detach_locked(core, &mut st);
            }
            if let Some(peer) = st.pending_call.as_ref().map(|p| p.peer) {
                match call.as_ref() {
                    Some(c) if c.peer == peer && c.state == CallState::Active => {
                        if let Some(p) = st.pending_call.take()
                            && let Err(e) = self.attach_locked(core, &mut st, p.session, Kind::Call { peer })
                        {
                            core.toast(ToastKind::Error, e);
                            // `st` is held here: hang up from a task.
                            hang_up_unattached(core);
                        }
                    }
                    Some(c) if c.peer == peer && c.state == CallState::RingingOut => {}
                    _ => st.pending_call = None,
                }
            }
        }
        self.sync_engine(&mut st, info.as_ref());
        self.refresh_room(core, &mut st);
        self.prune_watchers(core, &st);
        self.emit_voice(core, &mut st);
    }

    /// Follow track, epoch and path changes of the attached session (host migration).
    fn sync_engine(&self, st: &mut State, info: Option<&VoiceInfo>) {
        let (Some(a), Some(i)) = (st.attached.as_mut(), info) else { return };
        if i.conv != a.conv {
            return;
        }
        a.local_track = i.local_track;
        let engine = self.engine_handle();
        if a.epoch != i.epoch {
            a.epoch = i.epoch;
            if let Some(e) = &engine {
                e.set_epoch(i.epoch);
            }
        }
        if a.relayed != i.relayed {
            a.relayed = i.relayed;
            if let Some(e) = &engine {
                e.set_relayed(i.relayed);
            }
        }
    }

    /// Recompute short ids and room members; forget the tracks of members who left and
    /// re-apply per-user volumes.
    pub(super) fn refresh_room(&self, core: &Core, st: &mut State) {
        let Some(a) = st.attached.as_ref() else { return };
        let mut next = RoomMap::default();
        match a.kind {
            Kind::Room { server, channel } => {
                // Keep the last map while the server is looking for a host.
                if !roster_current(core.net.connection(&server).state) {
                    return;
                }
                let Some(r) = core.net.roster(&server) else { return };
                for e in &r.online {
                    next.short.insert(e.short_id, e.user);
                    if e.voice == Some(channel) && e.user != core.me {
                        next.users.insert(e.user);
                    }
                }
            }
            Kind::Call { peer } => {
                // The peer's voice packets carry track_id(call_short_id(peer, me), ..).
                next.short.insert(tincan_net::call_short_id(&peer, &core.me), peer);
                next.users.insert(peer);
            }
        }
        if next == st.room {
            return;
        }
        let engine = self.engine_handle();
        let left: Vec<(UserId, Option<u16>)> =
            st.room.users.difference(&next.users).map(|u| (*u, st.room.short_of(u))).collect();
        for (user, short) in left {
            if let (Some(e), Some(short)) = (&engine, short) {
                e.remove_track(track_id(short, KIND_VOICE));
            }
            engine::set_speaking(core, st, user, false);
        }
        st.room = next;
        if let Some(e) = &engine {
            self.apply_volumes(e, st);
        }
    }

    /// Per-user playback gains for everyone in my session.
    pub(super) fn apply_volumes(&self, e: &tincan_audio::AudioEngine, st: &State) {
        for user in &st.room.users {
            if let Some(short) = st.room.short_of(user) {
                e.set_track_volume(track_id(short, KIND_VOICE), st.volumes.get(user).copied().unwrap_or(1.0));
            }
        }
    }

    /// Screen tracks shared in my session right now (None while that is unknown).
    pub(super) fn live_tracks(&self, core: &Core, st: &State) -> Option<HashSet<u16>> {
        let a = st.attached.as_ref()?;
        let mut live = HashSet::new();
        if let Some(t) = core.net.voice_info().filter(|i| i.conv == a.conv).and_then(|i| i.sharing) {
            live.insert(t);
        }
        match a.kind {
            Kind::Room { server, channel } => {
                if !roster_current(core.net.connection(&server).state) {
                    return None;
                }
                let r = core.net.roster(&server)?;
                live.extend(r.online.iter().filter(|e| e.voice == Some(channel)).filter_map(|e| e.sharing));
            }
            Kind::Call { peer } => {
                if let Some(t) = core.net.call_info().filter(|c| c.peer == peer).and_then(|c| c.peer_sharing) {
                    live.insert(t);
                }
            }
        }
        Some(live)
    }

    /// Stop delivering tracks that are no longer shared.
    pub(super) fn prune_watchers(&self, core: &Core, st: &State) {
        let Some(live) = self.live_tracks(core, st) else { return };
        let gone: Vec<u16> = {
            let mut w = self.watchers.lock();
            let gone: Vec<u16> = w.keys().filter(|t| !live.contains(t)).copied().collect();
            for t in &gone {
                w.remove(t);
            }
            gone
        };
        for t in gone {
            core.net.unsubscribe_track(t);
        }
    }

    /// Emit `voice-updated` if the session DTO differs from the last one sent.
    pub(super) fn emit_voice(&self, core: &Core, st: &mut State) {
        let dto = self.voice_dto(core, st);
        self.room_cue(core, st, dto.as_ref());
        let Ok(v) = serde_json::to_value(&dto) else { return };
        if st.last_voice.as_ref() == Some(&v) {
            return;
        }
        st.last_voice = Some(v.clone());
        core.events.emit_value(Ev::VoiceUpdated, v);
    }

    /// Join and leave sounds (`cues.rs`): compares the room's people with the last emit.
    fn room_cue(&self, core: &Core, st: &mut State, dto: Option<&dto::VoiceSession>) {
        // While the server's roster is not current (a host change in progress) the session
        // lists only me: keep the last membership instead of cueing everyone out and back in.
        if let Some(a) = st.attached.as_ref()
            && let Kind::Room { server, .. } = a.kind
            && dto.is_some()
            && !roster_current(core.net.connection(&server).state)
        {
            return;
        }
        let me = core.me.to_z32();
        let next: super::cues::Seen = dto.filter(|v| matches!(v.kind, dto::VoiceKind::Room)).map(|v| {
            (v.conv_id.clone(), v.participants.iter().map(|p| p.user_id.clone()).filter(|u| *u != me).collect())
        });
        let cue = super::cues::cue_for(&st.last_room, &next);
        st.last_room = next;
        let Some(cue) = cue else { return };
        // Members joining and leaving over and over must not turn into a stream of sounds.
        let now = std::time::Instant::now();
        if st.last_cue.is_some_and(|t| now.duration_since(t) < super::cues::MIN_GAP) {
            return;
        }
        if !st.deafened
            && let Some(engine) = self.engine_handle()
            && let Some(pcm) = super::cues::pcm(cue)
        {
            st.last_cue = Some(now);
            engine.play_local(pcm);
        }
    }

    /// `VoiceSession` of the attached session.
    pub(super) fn voice_dto(&self, core: &Core, st: &State) -> Option<dto::VoiceSession> {
        let a = st.attached.as_ref()?;
        let info = core.net.voice_info().filter(|i| i.conv == a.conv);
        let connected = info.as_ref().is_some_and(|i| i.connected);
        let my_share = info.as_ref().and_then(|i| i.sharing);
        // To the host in a room (None while I host it), to the peer in a call. Net updates
        // it about once a second and reports only real moves, so it does not churn the DTO.
        let rtt_ms = info.as_ref().filter(|i| i.connected).and_then(|i| i.rtt_ms);
        // `Connection.path` is None while hosting; the session path says what media rides on.
        let path = match (connected, info.as_ref().is_some_and(|i| i.relayed)) {
            (false, _) => dto::PathKind::None,
            (true, true) => dto::PathKind::Relay,
            (true, false) => dto::PathKind::Direct,
        };
        let me = dto::VoiceParticipant {
            user_id: core.me.to_z32(),
            name: core.user_name(&core.me),
            muted: st.muted,
            deafened: st.deafened,
            speaking: st.speaking.contains(&core.me),
            avatar: core.my_avatar_url(),
            screen_track: my_share,
            volume: 1.0,
        };
        let mut participants = vec![me];
        let session = match a.kind {
            Kind::Room { server, channel } => {
                let view = core.view(&server)?;
                let conn = core.net.connection(&server);
                if roster_current(conn.state)
                    && let Some(r) = core.net.roster(&server)
                {
                    for e in r.online.iter().filter(|e| e.voice == Some(channel) && e.user != core.me) {
                        participants.push(dto::VoiceParticipant {
                            user_id: e.user.to_z32(),
                            name: core.author_name(&view, &e.user),
                            muted: e.muted,
                            deafened: e.deafened,
                            speaking: st.speaking.contains(&e.user),
                            avatar: core.member_avatar_url(&view, &e.user),
                            screen_track: e.sharing,
                            volume: volume_of(st, &e.user),
                        });
                    }
                }
                // A member's path comes from the connection when it knows one.
                let path = if connected && conn.state == ConnState::Member && path_kind(conn.path) != dto::PathKind::None {
                    path_kind(conn.path)
                } else {
                    path
                };
                dto::VoiceSession {
                    kind: dto::VoiceKind::Room,
                    conv_id: server.to_hex(),
                    channel_id: Some(channel.to_hex()),
                    name: view.all_channels.get(&channel).map(|c| crate::views::display(&c.name, "unnamed")).unwrap_or_default(),
                    participants,
                    muted: st.muted,
                    deafened: st.deafened,
                    sharing: my_share.is_some(),
                    path,
                    rtt_ms,
                }
            }
            Kind::Call { peer } => {
                let call = core.net.call_info().filter(|c| c.peer == peer);
                participants.push(dto::VoiceParticipant {
                    user_id: peer.to_z32(),
                    name: core.user_name(&peer),
                    muted: call.as_ref().is_some_and(|c| c.peer_muted),
                    deafened: call.as_ref().is_some_and(|c| c.peer_deafened),
                    speaking: st.speaking.contains(&peer),
                    avatar: core.dm_avatar_url(&peer),
                    screen_track: call.as_ref().and_then(|c| c.peer_sharing),
                    volume: volume_of(st, &peer),
                });
                dto::VoiceSession {
                    kind: dto::VoiceKind::Call,
                    conv_id: a.conv.to_hex(),
                    channel_id: None,
                    name: core.user_name(&peer),
                    participants,
                    muted: st.muted,
                    deafened: st.deafened,
                    sharing: my_share.is_some(),
                    path,
                    rtt_ms,
                }
            }
        };
        let mut session = session;
        session.participants.sort_by(|x, y| x.name.to_lowercase().cmp(&y.name.to_lowercase()).then_with(|| x.user_id.cmp(&y.user_id)));
        Some(session)
    }

    /// `user`'s display name changed: re-send the call and voice DTOs that show it.
    pub(crate) fn on_name_changed(&self, core: &Arc<Core>, user: UserId) {
        if self.is_closed() {
            return;
        }
        let call = core.net.call_info().filter(|c| c.peer == user);
        if let Some(d) = call.as_ref().and_then(|c| call_dto(core, c)) {
            core.events.emit(Ev::CallUpdated, d);
        }
        let mut st = self.st.lock();
        if st.attached.is_some() {
            self.emit_voice(core, &mut st);
        }
    }

    /// A server's roster changed.
    pub(super) fn on_roster(&self, core: &Arc<Core>, server: ServerId) {
        let mut st = self.st.lock();
        if !st.attached.as_ref().is_some_and(|a| matches!(a.kind, Kind::Room { server: s, .. } if s == server)) {
            return;
        }
        self.refresh_room(core, &mut st);
        self.prune_watchers(core, &st);
        self.emit_voice(core, &mut st);
    }

    /// A server's connection changed (path, RTT, host).
    pub(super) fn on_connection(&self, core: &Arc<Core>, server: ServerId) {
        let mut st = self.st.lock();
        if st.attached.as_ref().is_some_and(|a| matches!(a.kind, Kind::Room { server: s, .. } if s == server)) {
            self.refresh_room(core, &mut st);
            self.emit_voice(core, &mut st);
        }
    }

    /// A call changed state: `call-updated` (null once it ended, with a notice when that
    /// is news), then reconcile.
    pub(super) fn on_call_updated(&self, core: &Arc<Core>, call: &CallInfo) {
        let prev = {
            let mut st = self.st.lock();
            let prev = st.last_call.filter(|(p, _)| *p == call.peer).map(|(_, s)| s);
            st.last_call = (call.state != CallState::Ended).then_some((call.peer, call.state));
            prev
        };
        match call_dto(core, call) {
            Some(d) => core.events.emit(Ev::CallUpdated, d),
            None => {
                core.events.emit_value(Ev::CallUpdated, Value::Null);
                if let Some((kind, text)) = end_notice(call.ended, prev, &core.user_name(&call.peer)) {
                    core.toast(kind, text);
                }
            }
        }
        self.reconcile(core);
    }

    // ------------------------------------------------------------------ pumps

    /// Received audio -> the current engine (whichever engine is running, so a restarted
    /// engine keeps receiving).
    fn spawn_audio_pump(&self, mut rx: mpsc::Receiver<Bytes>) -> AbortHandle {
        let slot = self.engine.clone();
        let counters = self.counters.clone();
        tokio::spawn(async move {
            let mut batch = Vec::with_capacity(32);
            while rx.recv_many(&mut batch, 32).await > 0 {
                let engine = slot.lock().engine.clone();
                match engine {
                    Some(e) => {
                        counters.audio_in.fetch_add(batch.len() as u64, Ordering::Relaxed);
                        batch.drain(..).for_each(|p| e.push_packet(p));
                    }
                    None => batch.clear(),
                }
            }
        })
        .abort_handle()
    }

    /// Received video -> the `screen_watch` sink of its track.
    fn spawn_video_pump(&self, net: Net, mut rx: mpsc::Receiver<(u16, VideoFrame)>) -> AbortHandle {
        let watchers = self.watchers.clone();
        let counters = self.counters.clone();
        tokio::spawn(async move {
            while let Some((track, frame)) = rx.recv().await {
                let stopped = {
                    let mut w = watchers.lock();
                    match w.get_mut(&track) {
                        Some(sink) => {
                            let keep = sink(ui_video_message(track, &frame));
                            if keep {
                                counters.video_out.fetch_add(1, Ordering::Relaxed);
                            } else {
                                w.remove(&track);
                            }
                            !keep
                        }
                        None => false,
                    }
                };
                if stopped {
                    net.unsubscribe_track(track);
                }
            }
        })
        .abort_handle()
    }

    // ------------------------------------------------------------------ commands

    /// `voice_join`.
    pub(super) async fn voice_join(&self, core: &Arc<Core>, server: ServerId, channel: ChannelId) -> Result<dto::VoiceSession, String> {
        let _ops = self.ops.lock().await;
        let view = core.server_view(&server)?;
        match view.all_channels.get(&channel) {
            Some(c) if !c.deleted && c.kind == tincan_core::event::ChannelKind::Voice => {}
            Some(c) if !c.deleted => return Err("That's a text channel. Pick a voice room.".into()),
            _ => return Err("That voice room doesn't exist anymore.".into()),
        }
        {
            let st = self.st.lock();
            if st.attached.as_ref().is_some_and(|a| a.kind == Kind::Room { server, channel })
                && let Some(v) = self.voice_dto(core, &st)
            {
                return Ok(v);
            }
        }
        let sw = self.begin_switch();
        let res = core.net.voice_join(server, channel).await;
        let out = {
            let mut st = self.st.lock();
            sw.finish(&mut st);
            match res {
                Ok(session) => self
                    .attach_locked(core, &mut st, session, Kind::Room { server, channel })
                    .and_then(|()| self.voice_dto(core, &st).ok_or_else(|| "Couldn't join the voice room. Try again.".to_string())),
                Err(e) => Err(net_err(e)),
            }
        };
        if out.is_err() && core.net.voice_info().is_some_and(|i| i.conv == server && i.channel == Some(channel)) {
            core.net.voice_leave().await;
        }
        self.reconcile(core);
        out
    }

    /// `voice_leave` (also hangs up an active call, like `Net::voice_leave`).
    pub(super) async fn voice_leave(&self, core: &Arc<Core>) {
        let _ops = self.ops.lock().await;
        self.leave_now(core).await;
    }

    /// Leave the room or call, with `ops` held.
    async fn leave_now(&self, core: &Arc<Core>) {
        core.net.voice_leave().await;
        {
            let mut st = self.st.lock();
            self.detach_locked(core, &mut st);
        }
        self.reconcile(core);
    }

    /// My voice room, if it is one of `server`'s and the owner deleted it.
    fn deleted_room(&self, core: &Core, server: ServerId) -> Option<ChannelId> {
        let channel = match self.st.lock().attached.as_ref()?.kind {
            Kind::Room { server: s, channel } if s == server => channel,
            _ => return None,
        };
        let view = core.view(&server)?;
        view.all_channels.get(&channel).is_some_and(|c| c.deleted).then_some(channel)
    }

    /// The channels of `server` changed: if the owner deleted the voice room I am in, leave
    /// it (the room is gone for everyone, and nobody else could see who is still talking).
    pub(crate) fn on_channels(&self, core: &Arc<Core>, server: ServerId) {
        if self.is_closed() || self.deleted_room(core, server).is_none() {
            return;
        }
        let core = core.clone();
        tokio::spawn(async move {
            let media = &core.media;
            let _ops = media.ops.lock().await;
            // Checked again with `ops` held: a command may have switched rooms meanwhile.
            if media.is_closed() || media.deleted_room(&core, server).is_none() {
                return;
            }
            tracing::info!(server = %server.short(), "my voice room was deleted; leaving it");
            media.leave_now(&core).await;
            core.toast(ToastKind::Info, "The voice room was deleted.");
        });
    }

    /// Is my voice room one of `server`'s?
    fn room_in(&self, server: ServerId) -> bool {
        matches!(self.st.lock().attached.as_ref().map(|a| &a.kind), Some(Kind::Room { server: s, .. }) if *s == server)
    }

    /// The owner removed me from `server`: leave its voice room if I am in it.
    pub(crate) fn on_removed(&self, core: &Arc<Core>, server: ServerId) {
        if self.is_closed() || !self.room_in(server) {
            return;
        }
        let core = core.clone();
        tokio::spawn(async move {
            let media = &core.media;
            let _ops = media.ops.lock().await;
            if media.is_closed() || !media.room_in(server) {
                return;
            }
            media.leave_now(&core).await;
        });
    }

    /// `call_start`: returns the ringing call; the engine starts once it is accepted.
    pub(super) async fn call_start(&self, core: &Arc<Core>, user: UserId) -> Result<dto::CallInfo, String> {
        if user == core.me {
            return Err("You can't call yourself.".into());
        }
        let _ops = self.ops.lock().await;
        let sw = self.begin_switch();
        let res = core.net.call_start(user).await;
        let out = {
            let mut st = self.st.lock();
            sw.finish(&mut st);
            match res {
                Ok((info, session)) => {
                    let d = call_dto(core, &info);
                    if info.state == CallState::Active {
                        // They were ringing me: this answered their call.
                        st.pending_call = None;
                        let attached = self.attach_locked(core, &mut st, session, Kind::Call { peer: info.peer });
                        if attached.is_err() {
                            hang_up_unattached(core);
                        }
                        attached.map(|()| d)
                    } else {
                        st.pending_call = Some(PendingCall { peer: info.peer, session });
                        Ok(d)
                    }
                }
                Err(e) => Err(net_err(e)),
            }
        };
        self.reconcile(core);
        out?.ok_or_else(|| "That call has ended.".to_string())
    }

    /// `call_accept`.
    pub(super) async fn call_accept(&self, core: &Arc<Core>, call_id: &str) -> Result<dto::VoiceSession, String> {
        let ended = || "That call has ended.".to_string();
        let id: u64 = call_id.trim().parse().map_err(|_| ended())?;
        let _ops = self.ops.lock().await;
        let sw = self.begin_switch();
        let res = core.net.call_accept(id).await;
        let peer = core.net.call_info().filter(|c| c.state == CallState::Active).map(|c| c.peer);
        let out = {
            let mut st = self.st.lock();
            sw.finish(&mut st);
            match (res, peer) {
                (Ok(session), Some(peer)) => {
                    st.pending_call = None;
                    self.attach_locked(core, &mut st, session, Kind::Call { peer }).and_then(|()| self.voice_dto(core, &st).ok_or_else(ended))
                }
                (Ok(_), None) => Err(ended()),
                (Err(e), _) => Err(net_err(e)),
            }
        };
        let in_call = self.st.lock().attached.as_ref().is_some_and(|a| matches!(a.kind, Kind::Call { .. }));
        if out.is_err() && peer.is_some() && !in_call {
            // Net answered but the audio could not start: end the call instead of leaving
            // both sides in silence.
            core.net.call_hangup().await;
        }
        self.reconcile(core);
        out
    }

    /// `call_decline`.
    pub(super) async fn call_decline(&self, core: &Arc<Core>, call_id: &str) {
        if let Ok(id) = call_id.trim().parse::<u64>() {
            core.net.call_decline(id).await;
        }
    }

    /// `call_hangup`: hangs up, cancels my ringing call, or declines an incoming one.
    pub(super) async fn call_hangup(&self, core: &Arc<Core>) {
        let _ops = self.ops.lock().await;
        core.net.call_hangup().await;
        self.st.lock().pending_call = None;
        self.reconcile(core);
    }

    /// `set_muted`: unmuting also undeafens (the UI's model).
    pub(super) fn set_muted(&self, core: &Arc<Core>, muted: bool) {
        let mut st = self.st.lock();
        st.muted = muted;
        if !muted {
            st.deafened = false;
        }
        self.push_mute_state(core, &mut st);
    }

    /// `set_deafened`: deafening also mutes; undeafening leaves the mic muted.
    pub(super) fn set_deafened(&self, core: &Arc<Core>, deafened: bool) {
        let mut st = self.st.lock();
        st.deafened = deafened;
        if deafened {
            st.muted = true;
        }
        self.push_mute_state(core, &mut st);
    }

    fn push_mute_state(&self, core: &Core, st: &mut State) {
        if let Some(e) = self.engine_handle() {
            e.set_muted(st.muted);
            e.set_deafened(st.deafened);
        }
        let (muted, deafened) = (st.muted, st.deafened);
        core.net.set_status(|s| {
            s.muted = muted;
            s.deafened = deafened;
        });
        self.emit_voice(core, st);
    }

    /// `set_user_volume` (0..2, remembered across sessions and restarts).
    pub(super) fn set_user_volume(&self, core: &Arc<Core>, user: UserId, volume: f64) -> Result<(), String> {
        if !volume.is_finite() {
            return Err("Volume must be a number between 0 and 2.".into());
        }
        let v = volume.clamp(0.0, 2.0) as f32;
        let mut st = self.st.lock();
        if (v - 1.0).abs() < 1e-4 {
            st.volumes.remove(&user);
        } else {
            st.volumes.insert(user, v);
        }
        save_volumes(core, &st.volumes);
        if let (Some(e), Some(short)) = (self.engine_handle(), st.room.short_of(&user)) {
            e.set_track_volume(track_id(short, KIND_VOICE), v);
        }
        // `VoiceParticipant.volume` changed.
        self.emit_voice(core, &mut st);
        Ok(())
    }

    /// `audio_devices` (empty with `no_devices`).
    pub(super) async fn audio_devices(&self) -> Result<dto::AudioDevices, String> {
        if self.no_devices {
            return Ok(dto::AudioDevices::default());
        }
        let failed = || "Couldn't list your audio devices. Check that Windows audio is running.".to_string();
        let list = tokio::task::spawn_blocking(tincan_audio::AudioEngine::devices).await.map_err(|_| failed())?.map_err(|e| {
            tracing::warn!(error = %e, "listing audio devices failed");
            failed()
        })?;
        let map = |d: &tincan_audio::DeviceInfo| dto::AudioDevice { id: d.id.clone(), name: d.name.clone(), is_default: d.is_default };
        Ok(dto::AudioDevices { inputs: list.inputs.iter().map(map).collect(), outputs: list.outputs.iter().map(map).collect() })
    }

    /// `mic_test`: `mic-level` events about 20 times per second while on.
    pub(super) fn mic_test(&self, core: &Arc<Core>, on: bool) -> Result<(), String> {
        let mut st = self.st.lock();
        if on {
            st.mic_test = true;
            match self.ensure_engine(core, &st) {
                Ok(e) => e.mic_test(true),
                Err(e) => {
                    st.mic_test = false;
                    return Err(e);
                }
            }
        } else {
            st.mic_test = false;
            if let Some(e) = self.engine_handle() {
                e.mic_test(false);
            }
        }
        Ok(())
    }
}
