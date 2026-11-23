//! The media hub: which role each server is in (host SFU or member link), the local room
//! session, the 1:1 call state machine, and routing of media control messages.
//!
//! The session actors call in through drop guards ([`HostMedia`] in the host state,
//! [`MemberMedia`] in the member state), so every way out of a role (handoff, merge, crash
//! failover, shutdown) tears its media down. Guards carry a token or the connection's
//! `stable_id`, so a late drop never removes its successor's registration.
//!
//! Lock order: hub, then an SFU or the DM table. Neither calls back into the hub while
//! holding its own lock.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use bytes::Bytes;
use iroh::endpoint::Connection;
use parking_lot::Mutex;
use tincan_core::event::ChannelKind;
use tincan_core::keys::random_bytes;
use tincan_core::media::{KIND_SCREEN, VideoFrame, track_id, track_short_id};
use tincan_core::wire::{ClipInfo, Ctl};
use tincan_core::{ChannelId, ConvId, ServerId, UserId};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use super::recv::Pause;
use super::sfu::Sfu;
use super::voice::{self, Transport, VoiceCmd, VoiceHandle, VoiceKind};
use super::wire::CtlTx;
use super::{CallEnd, CallInfo, CallState, MediaSession, SHARE_QUEUE, VoiceInfo, call_short_id};
use crate::dm::DmHold;
use crate::error::{NetError, Result};
use crate::events::NetEvent;
use crate::hooks::CtlSource;
use crate::inner::Inner;

/// Media role of one server.
#[derive(Clone)]
enum Role {
    Member { host: UserId, conn: Connection, ctl: CtlTx, epoch: u64, short_id: u16 },
    Host { sfu: Arc<Sfu>, token: u64, epoch: u64, short_id: u16 },
}

impl Role {
    fn transport(&self) -> Transport {
        match self {
            Role::Member { host, conn, ctl, epoch, short_id } => {
                Transport::Remote { conn: conn.clone(), ctl: ctl.clone(), peer: *host, epoch: *epoch, short_id: *short_id }
            }
            Role::Host { sfu, epoch, short_id, .. } => Transport::Sfu { sfu: sfu.clone(), epoch: *epoch, short_id: *short_id },
        }
    }

    fn short_id(&self) -> u16 {
        match self {
            Role::Member { short_id, .. } | Role::Host { short_id, .. } => *short_id,
        }
    }

    fn epoch(&self) -> u64 {
        match self {
            Role::Member { epoch, .. } | Role::Host { epoch, .. } => *epoch,
        }
    }
}

struct Call {
    info: CallInfo,
    voice: Option<VoiceHandle>,
    hold: Option<DmHold>,
    timer: Option<AbortHandle>,
    link: Option<usize>,
}

#[derive(Default)]
struct State {
    roles: HashMap<ServerId, Role>,
    room: Option<VoiceHandle>,
    call: Option<Call>,
}

impl State {
    /// The session media commands go to: an active call, else the room.
    fn active(&self) -> Option<&VoiceHandle> {
        match &self.call {
            Some(c) if c.info.state == CallState::Active => c.voice.as_ref(),
            _ => self.room.as_ref(),
        }
    }
}

/// All media state of one `Net`.
pub(crate) struct MediaHub {
    st: Mutex<State>,
    next: AtomicU64,
    #[cfg(test)]
    pause_tx: tokio::sync::watch::Sender<bool>,
}

impl Default for MediaHub {
    fn default() -> Self {
        Self {
            st: Mutex::new(State::default()),
            next: AtomicU64::new(1),
            #[cfg(test)]
            pause_tx: tokio::sync::watch::channel(false).0,
        }
    }
}

impl std::fmt::Debug for MediaHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("MediaHub") }
}

/// Host role guard: the SFU (and the rendezvous publisher) live as long as it does.
pub(crate) struct HostMedia {
    net: Weak<Inner>,
    server: ServerId,
    token: u64,
    rdv: Option<AbortHandle>,
}

impl Drop for HostMedia {
    fn drop(&mut self) {
        if let Some(r) = self.rdv.take() {
            r.abort();
        }
        if let Some(net) = self.net.upgrade() {
            net.media.host_ended(self.server, self.token);
        }
    }
}

/// Member role guard: media rides this host connection while it lives.
pub(crate) struct MemberMedia {
    net: Weak<Inner>,
    server: ServerId,
    stable: usize,
}

impl Drop for MemberMedia {
    fn drop(&mut self) {
        if let Some(net) = self.net.upgrade() {
            net.media.member_ended(self.server, self.stable);
        }
    }
}

fn emit_all(net: &Inner, events: Vec<NetEvent>) {
    for e in events {
        net.emit(e);
    }
}

impl MediaHub {
    /// Pause handle for stream readers (a test hook; never pauses otherwise).
    pub fn pause(&self) -> Pause {
        #[cfg(test)]
        {
            Pause::new(self.pause_tx.subscribe())
        }
        #[cfg(not(test))]
        {
            Pause::new()
        }
    }

    /// Test hook: stop (true) or resume (false) reading every incoming video stream.
    #[cfg(test)]
    pub fn set_paused(&self, paused: bool) { self.pause_tx.send_replace(paused); }

    // ------------------------------------------------------------------ roles

    /// This device became host of `server` (called while the host state is built).
    pub fn host_started(&self, net: &Arc<Inner>, server: ServerId, epoch: u64, short_id: u16, rdv: Option<AbortHandle>) -> HostMedia {
        let token = self.next.fetch_add(1, Ordering::Relaxed);
        let sfu = Sfu::new(net, server, short_id);
        let mut st = self.st.lock();
        let role = Role::Host { sfu, token, epoch, short_id };
        if let Some(r) = st.room.as_ref().filter(|r| matches!(r.kind, VoiceKind::Room { server: s, .. } if s == server)) {
            r.send(VoiceCmd::Transport(role.transport()));
        }
        if let Some(Role::Host { sfu: old, .. }) = st.roles.insert(server, role) {
            old.shutdown();
        }
        HostMedia { net: Arc::downgrade(net), server, token, rdv }
    }

    fn host_ended(&self, server: ServerId, token: u64) {
        let mut st = self.st.lock();
        let Some(Role::Host { token: t, .. }) = st.roles.get(&server) else { return };
        if *t != token {
            return;
        }
        if let Some(Role::Host { sfu, .. }) = st.roles.remove(&server) {
            sfu.shutdown();
        }
        if let Some(r) = st.room.as_ref().filter(|r| matches!(r.kind, VoiceKind::Room { server: s, .. } if s == server)) {
            r.send(VoiceCmd::Transport(Transport::None));
        }
    }

    /// This device became a member of `server`'s host over `conn`.
    #[allow(clippy::too_many_arguments)]
    pub fn member_started(
        &self,
        net: &Arc<Inner>,
        server: ServerId,
        host: UserId,
        conn: &Connection,
        live: mpsc::Sender<Bytes>,
        epoch: u64,
        short_id: u16,
    ) -> MemberMedia {
        let role = Role::Member { host, conn: conn.clone(), ctl: CtlTx(live), epoch, short_id };
        let mut st = self.st.lock();
        if let Some(r) = st.room.as_ref().filter(|r| matches!(r.kind, VoiceKind::Room { server: s, .. } if s == server)) {
            r.send(VoiceCmd::Transport(role.transport()));
        }
        if let Some(Role::Host { sfu, .. }) = st.roles.insert(server, role) {
            sfu.shutdown();
        }
        MemberMedia { net: Arc::downgrade(net), server, stable: conn.stable_id() }
    }

    fn member_ended(&self, server: ServerId, stable: usize) {
        let mut st = self.st.lock();
        let Some(Role::Member { conn, .. }) = st.roles.get(&server) else { return };
        if conn.stable_id() != stable {
            return;
        }
        st.roles.remove(&server);
        if let Some(r) = st.room.as_ref().filter(|r| matches!(r.kind, VoiceKind::Room { server: s, .. } if s == server)) {
            r.send(VoiceCmd::Transport(Transport::None));
        }
    }

    fn sfu(&self, server: &ServerId) -> Option<Arc<Sfu>> {
        match self.st.lock().roles.get(server) {
            Some(Role::Host { sfu, .. }) => Some(sfu.clone()),
            _ => None,
        }
    }

    /// Host side: a member's link came up.
    pub fn member_up(&self, server: ServerId, user: UserId, short_id: u16, conn: Connection, live: mpsc::Sender<Bytes>) {
        if let Some(sfu) = self.sfu(&server) {
            sfu.add_member(user, short_id, conn, CtlTx(live));
        }
    }

    /// Host side: a member's link (with this `stable_id`) went away.
    pub fn member_down(&self, server: ServerId, user: UserId, stable: usize) {
        if let Some(sfu) = self.sfu(&server) {
            sfu.remove_member(user, stable);
        }
    }

    // ------------------------------------------------------------------ control routing

    /// A media or call control message arrived (on a session or DM actor: never blocks).
    pub fn on_ctl(&self, net: &Arc<Inner>, source: CtlSource, ctl: &Ctl) {
        match source {
            CtlSource::FromMember { server, member, short_id } => {
                if let Some(sfu) = self.sfu(&server) {
                    sfu.on_ctl(net, member, short_id, ctl.clone());
                }
            }
            CtlSource::FromHost { server, .. } => {
                let st = self.st.lock();
                if let Some(r) = st.room.as_ref().filter(|r| matches!(r.kind, VoiceKind::Room { server: s, .. } if s == server)) {
                    r.send(VoiceCmd::FromPeer(ctl.clone()));
                }
            }
            CtlSource::Dm { peer } => self.on_dm_ctl(net, peer, ctl),
        }
    }

    fn on_dm_ctl(&self, net: &Arc<Inner>, peer: UserId, ctl: &Ctl) {
        let mut events = Vec::new();
        {
            let mut st = self.st.lock();
            match *ctl {
                Ctl::CallInvite { call_id } => self.on_invite(net, &mut st, peer, call_id, &mut events),
                Ctl::CallAccept { call_id } => {
                    let ringing = st.call.as_ref().is_some_and(|c| c.info.peer == peer && c.info.call_id == call_id && c.info.state == CallState::RingingOut);
                    if ringing {
                        activate(net, &mut st, &mut events);
                    }
                }
                Ctl::CallDecline { call_id } => {
                    if st.call.as_ref().is_some_and(|c| c.info.peer == peer && c.info.call_id == call_id && c.info.state == CallState::RingingOut) {
                        end_call(&mut st, CallEnd::Declined, &mut events);
                    }
                }
                Ctl::CallEnd { call_id } => {
                    if let Some(c) = st.call.as_ref().filter(|c| c.info.peer == peer && c.info.call_id == call_id) {
                        let why = if c.info.state == CallState::RingingIn { CallEnd::Cancelled } else { CallEnd::Hangup };
                        end_call(&mut st, why, &mut events);
                    }
                }
                Ctl::CallState { call_id, muted, deafened, sharing } => {
                    if let Some(c) = st.call.as_mut().filter(|c| c.info.peer == peer && c.info.call_id == call_id) {
                        let theirs = track_id(call_short_id(&peer, &net.me), KIND_SCREEN);
                        let sharing = sharing.filter(|t| *t == theirs);
                        if (c.info.peer_muted, c.info.peer_deafened, c.info.peer_sharing) != (muted, deafened, sharing) {
                            c.info.peer_muted = muted;
                            c.info.peer_deafened = deafened;
                            c.info.peer_sharing = sharing;
                            events.push(NetEvent::CallUpdated { call: c.info.clone() });
                        }
                    }
                }
                _ => {
                    if let Some(v) = st.call.as_ref().filter(|c| c.info.peer == peer && c.info.state == CallState::Active).and_then(|c| c.voice.as_ref()) {
                        v.send(VoiceCmd::FromPeer(ctl.clone()));
                    }
                }
            }
        }
        emit_all(net, events);
    }

    fn on_invite(&self, net: &Arc<Inner>, st: &mut State, peer: UserId, call_id: u64, events: &mut Vec<NetEvent>) {
        match st.call.as_mut() {
            Some(c) if c.info.peer == peer && c.info.state == CallState::RingingOut => {
                // Both called at once: the call of the lower user id wins.
                if peer < net.me {
                    c.info.call_id = call_id;
                    if let Some(v) = &c.voice {
                        v.send(VoiceCmd::CallId(call_id));
                    }
                    net.dm.send(&peer, &Ctl::CallAccept { call_id });
                    activate(net, st, events);
                }
            }
            Some(_) => {
                // Busy in another call.
                net.dm.send(&peer, &Ctl::CallDecline { call_id });
            }
            _ => {
                let info = CallInfo {
                    call_id,
                    peer,
                    conv: ConvId::dm(&net.me, &peer),
                    state: CallState::RingingIn,
                    ended: None,
                    peer_muted: false,
                    peer_deafened: false,
                    peer_sharing: None,
                };
                let hold = net.dm.hold(net, peer);
                let link = net.dm.connection(&peer).map(|c| c.stable_id());
                let timer = Some(ring_timer(net, call_id));
                events.push(NetEvent::CallInvite { call: info.clone() });
                events.push(NetEvent::CallUpdated { call: info.clone() });
                st.call = Some(Call { info, voice: None, hold: Some(hold), timer, link });
            }
        }
    }

    /// The DM connection to `peer` came up, was replaced or went away.
    pub fn dm_link(&self, net: &Arc<Inner>, peer: UserId) {
        let mut events = Vec::new();
        {
            let mut st = self.st.lock();
            let Some(c) = st.call.as_mut().filter(|c| c.info.peer == peer) else { return };
            match net.dm.live(&peer) {
                None => end_call(&mut st, CallEnd::Lost, &mut events),
                Some((conn, live)) if c.link != Some(conn.stable_id()) => {
                    c.link = Some(conn.stable_id());
                    if c.info.state == CallState::Active
                        && let Some(v) = &c.voice
                    {
                        let short_id = call_short_id(&net.me, &peer);
                        v.send(VoiceCmd::Transport(Transport::Remote { conn, ctl: CtlTx(live), peer, epoch: 0, short_id }));
                    }
                }
                Some(_) => {}
            }
        }
        emit_all(net, events);
    }

    fn ring_timeout(&self, net: &Arc<Inner>, call_id: u64) {
        let mut events = Vec::new();
        {
            let mut st = self.st.lock();
            let Some(c) = st.call.as_ref().filter(|c| c.info.call_id == call_id) else { return };
            match c.info.state {
                CallState::RingingOut => {
                    net.dm.send(&c.info.peer, &Ctl::CallEnd { call_id });
                    end_call(&mut st, CallEnd::NoAnswer, &mut events);
                }
                CallState::RingingIn => end_call(&mut st, CallEnd::NoAnswer, &mut events),
                _ => {}
            }
        }
        emit_all(net, events);
    }

    /// My mute/deafen state changed.
    pub fn status_changed(&self) {
        let st = self.st.lock();
        if let Some(v) = st.call.as_ref().filter(|c| c.info.state == CallState::Active).and_then(|c| c.voice.as_ref()) {
            v.send(VoiceCmd::StatusChanged);
        }
    }

    // ------------------------------------------------------------------ rooms

    /// Join a voice room (leaving any other room and ending any call).
    pub async fn voice_join(&self, net: &Arc<Inner>, server: ServerId, channel: ChannelId) -> Result<MediaSession> {
        let ok = net.replica(&server).ok_or(NetError::UnknownConv)?.lock().state.channels.get(&channel).is_some_and(|c| c.kind == ChannelKind::Voice && !c.deleted);
        if !ok {
            return Err(NetError::Invalid("That is not a voice channel.".into()));
        }
        // An active or outgoing call ends; an incoming one keeps ringing.
        let calling = self.st.lock().call.as_ref().is_some_and(|c| c.info.state != CallState::RingingIn);
        if calling {
            self.hangup(net);
        }
        let old = self.st.lock().room.take();
        if let Some(old) = old {
            old.stop().await;
            net.emit(NetEvent::VoiceUpdated { conv: old.conv });
        }
        net.set_status(|s| {
            s.voice = Some(channel);
            s.sharing = false;
        });
        let session = {
            let mut st = self.st.lock();
            let role = st.roles.get(&server).cloned();
            let short = role.as_ref().map(Role::short_id).unwrap_or(0);
            let epoch = role.as_ref().map(Role::epoch).unwrap_or(0);
            let (handle, session) = voice::spawn(net, VoiceKind::Room { server, channel }, server, short, epoch, 0);
            if let Some(r) = role {
                handle.send(VoiceCmd::Transport(r.transport()));
            }
            if let Some(stale) = st.room.replace(handle) {
                stale.send(VoiceCmd::Stop(None));
            }
            session
        };
        net.emit(NetEvent::VoiceUpdated { conv: server });
        Ok(session)
    }

    /// Leave the voice room, or hang up the active call.
    pub async fn voice_leave(&self, net: &Arc<Inner>) {
        let in_call = self.st.lock().call.as_ref().is_some_and(|c| c.info.state == CallState::Active);
        if in_call {
            self.hangup(net);
            return;
        }
        let old = self.st.lock().room.take();
        if let Some(old) = old {
            old.stop().await;
            net.emit(NetEvent::VoiceUpdated { conv: old.conv });
        }
    }

    /// I left `server`: end my room session there, if any.
    pub async fn leave_server(&self, net: &Arc<Inner>, server: ServerId) {
        let old = {
            let mut st = self.st.lock();
            match &st.room {
                Some(r) if matches!(r.kind, VoiceKind::Room { server: s, .. } if s == server) => st.room.take(),
                _ => None,
            }
        };
        if let Some(old) = old {
            old.stop().await;
            net.emit(NetEvent::VoiceUpdated { conv: old.conv });
        }
    }

    /// State of the active session (an active call wins over the room).
    pub fn voice_info(&self) -> Option<VoiceInfo> { self.st.lock().active().map(|v| v.info.lock().clone()) }

    /// Start sharing in the active session.
    pub fn share_start(&self) -> Result<(u16, mpsc::Sender<VideoFrame>)> {
        let st = self.st.lock();
        let v = st.active().ok_or(NetError::NotInVoice)?;
        let short = track_short_id(v.info.lock().local_track);
        let (tx, rx) = mpsc::channel(SHARE_QUEUE);
        v.send(VoiceCmd::ShareStart(rx));
        Ok((track_id(short, KIND_SCREEN), tx))
    }

    /// Stop sharing.
    pub fn share_stop(&self) {
        if let Some(v) = self.st.lock().active() {
            v.send(VoiceCmd::ShareStop);
        }
    }

    /// Watch a track in the active session.
    pub fn subscribe(&self, track: u16) {
        if let Some(v) = self.st.lock().active() {
            v.send(VoiceCmd::Subscribe(track));
        }
    }

    /// Stop watching a track.
    pub fn unsubscribe(&self, track: u16) {
        if let Some(v) = self.st.lock().active() {
            v.send(VoiceCmd::Unsubscribe(track));
        }
    }

    /// Announce a soundboard clip I played to the active session.
    pub fn sound(&self, clip: ClipInfo) -> Result<()> {
        let st = self.st.lock();
        let v = st.active().ok_or(NetError::NotInVoice)?;
        v.send(VoiceCmd::Sound(clip));
        Ok(())
    }

    // ------------------------------------------------------------------ calls

    /// Ring `peer`. The session carries media once they accept.
    pub async fn call_start(&self, net: &Arc<Inner>, peer: UserId) -> Result<(CallInfo, MediaSession)> {
        if peer == net.me {
            return Err(NetError::Invalid("You can't call yourself.".into()));
        }
        if let Some(answer) = self.answer_ringing_from(net, peer) {
            return answer;
        }
        if self.st.lock().call.is_some() {
            return Err(NetError::Invalid("You're already in a call.".into()));
        }
        crate::dm::open_conv(net, peer)?;
        let hold = net.dm.hold(net, peer);
        let conn = net.dm.connect(net, peer).await?;
        let call_id = u64::from_le_bytes(random_bytes()).max(1);
        let conv = ConvId::dm(&net.me, &peer);
        let info = CallInfo {
            call_id,
            peer,
            conv,
            state: CallState::RingingOut,
            ended: None,
            peer_muted: false,
            peer_deafened: false,
            peer_sharing: None,
        };
        if let Some(answer) = self.answer_ringing_from(net, peer) {
            return answer;
        }
        let session = {
            let mut st = self.st.lock();
            if st.call.is_some() {
                return Err(NetError::Invalid("You're already in a call.".into()));
            }
            if !net.dm.send(&peer, &Ctl::CallInvite { call_id }) {
                return Err(NetError::Unreachable("Couldn't reach them right now.".into()));
            }
            let (handle, session) = voice::spawn(net, VoiceKind::Call { peer }, conv, call_short_id(&net.me, &peer), 0, call_id);
            let timer = Some(ring_timer(net, call_id));
            st.call = Some(Call { info: info.clone(), voice: Some(handle), hold: Some(hold), timer, link: Some(conn.stable_id()) });
            session
        };
        net.emit(NetEvent::CallUpdated { call: info.clone() });
        Ok((info, session))
    }

    /// Calling someone who is ringing me: answer their call instead.
    fn answer_ringing_from(&self, net: &Arc<Inner>, peer: UserId) -> Option<Result<(CallInfo, MediaSession)>> {
        let id = self.st.lock().call.as_ref().filter(|c| c.info.peer == peer && c.info.state == CallState::RingingIn).map(|c| c.info.call_id)?;
        Some(self.call_accept(net, id).map(|s| (self.call_info().unwrap_or_else(|| ended_info(net, peer, id)), s)))
    }

    /// Answer a ringing call.
    pub fn call_accept(&self, net: &Arc<Inner>, call_id: u64) -> Result<MediaSession> {
        let mut events = Vec::new();
        let res = {
            let mut st = self.st.lock();
            let ringing = st.call.as_ref().filter(|c| c.info.call_id == call_id && c.info.state == CallState::RingingIn).map(|c| c.info.peer);
            match ringing {
                None => Err(NetError::Invalid("That call is no longer ringing.".into())),
                Some(peer) if net.dm.live(&peer).is_none() => {
                    end_call(&mut st, CallEnd::Lost, &mut events);
                    Err(NetError::Unreachable("Couldn't reach them right now.".into()))
                }
                Some(peer) => {
                    let conv = ConvId::dm(&net.me, &peer);
                    let (handle, session) = voice::spawn(net, VoiceKind::Call { peer }, conv, call_short_id(&net.me, &peer), 0, call_id);
                    if let Some(c) = st.call.as_mut() {
                        c.voice = Some(handle);
                    }
                    net.dm.send(&peer, &Ctl::CallAccept { call_id });
                    activate(net, &mut st, &mut events);
                    Ok(session)
                }
            }
        };
        emit_all(net, events);
        res
    }

    /// Decline a ringing call.
    pub fn call_decline(&self, net: &Arc<Inner>, call_id: u64) {
        let mut events = Vec::new();
        {
            let mut st = self.st.lock();
            if let Some(c) = st.call.as_ref().filter(|c| c.info.call_id == call_id && c.info.state == CallState::RingingIn) {
                net.dm.send(&c.info.peer, &Ctl::CallDecline { call_id });
                end_call(&mut st, CallEnd::Declined, &mut events);
            }
        }
        emit_all(net, events);
    }

    /// Hang up (or cancel, or decline) the current call.
    pub fn hangup(&self, net: &Arc<Inner>) {
        let mut events = Vec::new();
        {
            let mut st = self.st.lock();
            if let Some(c) = st.call.as_ref() {
                let (peer, id) = (c.info.peer, c.info.call_id);
                let (ctl, why) = match c.info.state {
                    CallState::RingingIn => (Ctl::CallDecline { call_id: id }, CallEnd::Declined),
                    CallState::RingingOut => (Ctl::CallEnd { call_id: id }, CallEnd::Cancelled),
                    _ => (Ctl::CallEnd { call_id: id }, CallEnd::Hangup),
                };
                net.dm.send(&peer, &ctl);
                end_call(&mut st, why, &mut events);
            }
        }
        emit_all(net, events);
    }

    /// The current call, if any.
    pub fn call_info(&self) -> Option<CallInfo> { self.st.lock().call.as_ref().map(|c| c.info.clone()) }

    /// Shutdown: end the call, leave the room.
    pub async fn shutdown(&self, net: &Arc<Inner>) {
        self.hangup(net);
        let room = self.st.lock().room.take();
        if let Some(r) = room {
            r.stop().await;
        }
    }
}

/// Info of a call that already ended (it raced with an answer).
fn ended_info(net: &Inner, peer: UserId, call_id: u64) -> CallInfo {
    CallInfo {
        call_id,
        peer,
        conv: ConvId::dm(&net.me, &peer),
        state: CallState::Ended,
        ended: Some(CallEnd::Lost),
        peer_muted: false,
        peer_deafened: false,
        peer_sharing: None,
    }
}

/// Arm the ringing timeout of `call_id`.
fn ring_timer(net: &Arc<Inner>, call_id: u64) -> AbortHandle {
    let weak = Arc::downgrade(net);
    let after = net.timings.call_ring;
    net.tasks.spawn(async move {
        tokio::time::sleep(after).await;
        if let Some(n) = weak.upgrade() {
            n.media.ring_timeout(&n, call_id);
        }
    })
}

/// The current call becomes active: media starts on the DM connection, the room is left.
fn activate(net: &Arc<Inner>, st: &mut State, events: &mut Vec<NetEvent>) {
    let Some(c) = st.call.as_mut() else { return };
    let peer = c.info.peer;
    c.info.state = CallState::Active;
    if let Some(t) = c.timer.take() {
        t.abort();
    }
    match net.dm.live(&peer) {
        Some((conn, live)) => {
            c.link = Some(conn.stable_id());
            if let Some(v) = &c.voice {
                let short_id = call_short_id(&net.me, &peer);
                v.send(VoiceCmd::Transport(Transport::Remote { conn, ctl: CtlTx(live), peer, epoch: 0, short_id }));
            }
            events.push(NetEvent::CallUpdated { call: c.info.clone() });
            events.push(NetEvent::VoiceUpdated { conv: c.info.conv });
            if let Some(room) = st.room.take() {
                room.send(VoiceCmd::Stop(None));
                events.push(NetEvent::VoiceUpdated { conv: room.conv });
            }
        }
        None => end_call(st, CallEnd::Lost, events),
    }
}

/// End the current call locally.
fn end_call(st: &mut State, why: CallEnd, events: &mut Vec<NetEvent>) {
    let Some(mut c) = st.call.take() else { return };
    if let Some(t) = c.timer.take() {
        t.abort();
    }
    if let Some(v) = c.voice.take() {
        v.send(VoiceCmd::Stop(None));
        events.push(NetEvent::VoiceUpdated { conv: v.conv });
    }
    drop(c.hold.take());
    c.info.state = CallState::Ended;
    c.info.ended = Some(why);
    events.push(NetEvent::CallUpdated { call: c.info });
}
