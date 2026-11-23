//! The local voice session: one actor task per room session or call.
//!
//! It owns the far ends of the app's channels for the whole session and moves them between
//! transports as the role changes:
//! - `Remote`: a member of the server's host (or the peer of a call). Audio datagrams go out
//!   on that connection and are read from it; GOP streams are read into `video_in`; the share
//!   uploads through a [`VideoTrack`] whose one viewer is the host (or the peer, when it
//!   subscribed). Receiver reports go back every second.
//! - `Sfu`: this device hosts the server. Audio is injected into the SFU and room audio comes
//!   from it; the share becomes one of the SFU's tracks.
//! - `None`: seeking a host. Outgoing audio is dropped; nothing comes in.
//!
//! On every new transport it re-announces itself (`VoiceJoin`, its `Subscribe`s, a fresh
//! keyframe for its share), so a migration only costs the time to find the new host.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Weak};

use bytes::Bytes;
use iroh::endpoint::Connection;
use parking_lot::Mutex;
use tincan_core::media::{KIND_SCREEN, KIND_VOICE, VideoFrame, track_id, track_kind};
use tincan_core::wire::{ClipInfo, Ctl};
use tincan_core::{ChannelId, ConvId, ServerId, UserId};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use super::recv::{self, Pause, VideoSink, spawn_video_rx};
use super::sfu::{LocalPeer, MEDIA_TICK, Sfu};
use super::stats::RxStats;
use super::track::{KEYFRAME_EVERY, VideoTrack, ViewerId};
use super::wire::{CtlTx, OutError, VideoOut, stamp_audio};
use super::{AUDIO_IN_QUEUE, AUDIO_OUT_QUEUE, MediaSession, RELAYED_VIEWER_BPS, VIDEO_IN_QUEUE, VIDEO_MAX_BPS, VoiceInfo};
use crate::events::NetEvent;
use crate::inner::Inner;
use crate::link::Tasks;

/// What the session is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VoiceKind {
    /// A server voice room.
    Room {
        /// The server.
        server: ServerId,
        /// The room.
        channel: ChannelId,
    },
    /// A 1:1 call over the DM connection.
    Call {
        /// The other party.
        peer: UserId,
    },
}

/// How media flows right now.
pub(crate) enum Transport {
    /// Nowhere (seeking a host, call not active yet).
    None,
    /// Over a connection to the host (or the call peer).
    Remote {
        /// The connection.
        conn: Connection,
        /// Its control link.
        ctl: CtlTx,
        /// The host or peer.
        peer: UserId,
        /// Host epoch (0 in calls).
        epoch: u64,
        /// My short id.
        short_id: u16,
    },
    /// This device hosts the server.
    Sfu {
        /// The server's SFU.
        sfu: Arc<Sfu>,
        /// Host epoch.
        epoch: u64,
        /// My short id.
        short_id: u16,
    },
}

/// Commands to a voice session.
pub(crate) enum VoiceCmd {
    /// The transport changed.
    Transport(Transport),
    /// Start sharing frames from this channel.
    ShareStart(mpsc::Receiver<VideoFrame>),
    /// Stop sharing.
    ShareStop,
    /// Watch a video track.
    Subscribe(u16),
    /// Stop watching it.
    Unsubscribe(u16),
    /// A media message from the host or the call peer.
    FromPeer(Ctl),
    /// A watched track has a gap.
    NeedKeyframe(u16),
    /// I played a soundboard clip.
    Sound(ClipInfo),
    /// My mute/deafen state changed (calls announce it).
    StatusChanged,
    /// The call id changed (both sides called at once).
    CallId(u64),
    /// End the session.
    Stop(Option<oneshot::Sender<()>>),
}

/// Handle to a running session.
pub(crate) struct VoiceHandle {
    /// What it is.
    pub kind: VoiceKind,
    /// Server or DM conversation.
    pub conv: ConvId,
    /// Commands.
    pub cmd: mpsc::UnboundedSender<VoiceCmd>,
    /// Current state.
    pub info: Arc<Mutex<VoiceInfo>>,
}

impl VoiceHandle {
    /// Send a command (the session may already be gone).
    pub fn send(&self, c: VoiceCmd) { let _ = self.cmd.send(c); }

    /// Stop and wait (briefly) until it left.
    pub async fn stop(&self) {
        let (tx, rx) = oneshot::channel();
        if self.cmd.send(VoiceCmd::Stop(Some(tx))).is_ok() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), rx).await;
        }
    }
}

/// Start a session. `short_id`/`epoch` are the best known values (re-stamped later).
pub(crate) fn spawn(net: &Arc<Inner>, kind: VoiceKind, conv: ConvId, short_id: u16, epoch: u64, call_id: u64) -> (VoiceHandle, MediaSession) {
    let (audio_out, audio_out_rx) = mpsc::channel(AUDIO_OUT_QUEUE);
    let (audio_in_tx, audio_in) = mpsc::channel(AUDIO_IN_QUEUE);
    let (video_tx, video_in) = mpsc::channel(VIDEO_IN_QUEUE);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let local_track = track_id(short_id, KIND_VOICE);
    let epoch_byte = (epoch & 0xff) as u8;
    let (channel, peer) = match kind {
        VoiceKind::Room { channel, .. } => (Some(channel), None),
        VoiceKind::Call { peer } => (None, Some(peer)),
    };
    let info = Arc::new(Mutex::new(VoiceInfo { conv, channel, peer, local_track, epoch: epoch_byte, ..VoiceInfo::default() }));
    let actor = Voice {
        net: Arc::downgrade(net),
        tasks: net.tasks.clone(),
        me: net.me,
        kind,
        conv,
        call_id,
        cmd_tx: cmd_tx.clone(),
        audio_in: audio_in_tx,
        video_in: VideoOut::new(video_tx),
        info: info.clone(),
        transport: Transport::None,
        rx_tasks: Vec::new(),
        stats: Arc::new(RxStats::default()),
        subs: Arc::new(Mutex::new(HashSet::new())),
        own_voice: Arc::new(AtomicU16::new(local_track)),
        short_id,
        epoch: epoch_byte,
        share: None,
        peer_watching: false,
        last_loss: None,
        kf_asked: HashMap::new(),
        pause: net.media.pause(),
        report_at: None,
    };
    net.tasks.spawn(actor.run(cmd_rx, audio_out_rx));
    let session = MediaSession { conv, local_track, epoch: epoch_byte, audio_out, audio_in, video_in };
    (VoiceHandle { kind, conv, cmd: cmd_tx, info }, session)
}

struct Share {
    frames: Option<mpsc::Receiver<VideoFrame>>,
    track: Arc<VideoTrack>,
}

struct Voice {
    net: Weak<Inner>,
    tasks: Arc<Tasks>,
    me: UserId,
    kind: VoiceKind,
    conv: ConvId,
    call_id: u64,
    cmd_tx: mpsc::UnboundedSender<VoiceCmd>,
    audio_in: mpsc::Sender<Bytes>,
    video_in: VideoOut,
    info: Arc<Mutex<VoiceInfo>>,
    transport: Transport,
    rx_tasks: Vec<AbortHandle>,
    stats: Arc<RxStats>,
    subs: Arc<Mutex<HashSet<u16>>>,
    own_voice: Arc<AtomicU16>,
    short_id: u16,
    epoch: u8,
    share: Option<Share>,
    peer_watching: bool,
    last_loss: Option<u8>,
    kf_asked: HashMap<u16, Instant>,
    pause: Pause,
    report_at: Option<Instant>,
}

async fn next_frame(share: &mut Option<Share>) -> Option<VideoFrame> {
    match share.as_mut().and_then(|s| s.frames.as_mut()) {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_opt(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

impl Voice {
    async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<VoiceCmd>, mut audio_out: mpsc::Receiver<Bytes>) {
        let mut audio_open = true;
        loop {
            tokio::select! {
                biased;
                c = cmd_rx.recv() => match c {
                    None => {
                        self.leave();
                        break;
                    }
                    Some(VoiceCmd::Stop(reply)) => {
                        self.leave();
                        if let Some(r) = reply {
                            let _ = r.send(());
                        }
                        break;
                    }
                    Some(c) => self.on_cmd(c),
                },
                p = audio_out.recv(), if audio_open => match p {
                    Some(p) => self.send_audio(p),
                    None => audio_open = false,
                },
                f = next_frame(&mut self.share) => match f {
                    Some(f) => {
                        if let Some(s) = &self.share {
                            s.track.push(f);
                        }
                    }
                    // The app dropped the frame sender: the share is over.
                    None => self.stop_share(),
                },
                _ = sleep_opt(self.report_at) => self.report(),
            }
        }
    }

    fn net(&self) -> Option<Arc<Inner>> { self.net.upgrade() }

    fn emit(&self, ev: NetEvent) {
        if let Some(n) = self.net() {
            n.emit(ev);
        }
    }

    fn on_cmd(&mut self, c: VoiceCmd) {
        match c {
            VoiceCmd::Transport(t) => self.set_transport(t),
            VoiceCmd::ShareStart(rx) => self.start_share(rx),
            VoiceCmd::ShareStop => self.stop_share(),
            VoiceCmd::Subscribe(t) => self.subscribe(t),
            VoiceCmd::Unsubscribe(t) => self.unsubscribe(t),
            VoiceCmd::FromPeer(ctl) => self.on_peer_ctl(ctl),
            VoiceCmd::NeedKeyframe(t) => self.need_keyframe(t),
            VoiceCmd::Sound(clip) => self.sound(clip),
            VoiceCmd::StatusChanged => self.send_call_state(),
            VoiceCmd::CallId(id) => {
                self.call_id = id;
                self.send_call_state();
            }
            VoiceCmd::Stop(_) => {}
        }
    }

    fn ctl(&self) -> Option<&CtlTx> {
        match &self.transport {
            Transport::Remote { ctl, .. } => Some(ctl),
            _ => None,
        }
    }

    /// Drop the current transport's tasks and registrations.
    fn teardown(&mut self) {
        for t in self.rx_tasks.drain(..) {
            t.abort();
        }
        if let Transport::Sfu { sfu, .. } = std::mem::replace(&mut self.transport, Transport::None) {
            sfu.clear_local();
            if self.share.is_some() {
                sfu.detach_local_track();
            }
        }
        if let Some(s) = &self.share {
            s.track.clear();
        }
        self.report_at = None;
    }

    fn set_ids(&mut self, short_id: u16, epoch: u64) {
        self.epoch = (epoch & 0xff) as u8;
        if short_id != self.short_id {
            self.short_id = short_id;
            self.own_voice.store(track_id(short_id, KIND_VOICE), Ordering::Relaxed);
            if let Some(s) = &mut self.share {
                s.track.end();
                s.track.clear();
                let track = new_share_track(&self.net, &self.tasks, short_id);
                s.track = track;
            }
        }
    }

    fn set_transport(&mut self, t: Transport) {
        self.teardown();
        self.transport = t;
        // A new path may replay the cached group the app already has.
        self.video_in.arm();
        let subs: Vec<u16> = self.subs.lock().iter().copied().collect();
        match &self.transport {
            Transport::None => {}
            Transport::Remote { conn, ctl, peer, epoch, short_id } => {
                let (conn, ctl, peer) = (conn.clone(), ctl.clone(), *peer);
                let (short_id, epoch) = (*short_id, *epoch);
                self.set_ids(short_id, epoch);
                self.rx_tasks.push(self.tasks.spawn(recv::audio_rx(conn.clone(), self.own_voice.clone(), self.audio_in.clone(), self.stats.clone())));
                let sink = Arc::new(SessionSink { video_in: self.video_in.clone(), subs: self.subs.clone(), stats: self.stats.clone(), cmd: self.cmd_tx.clone() });
                self.rx_tasks.push(spawn_video_rx(&self.tasks, conn.clone(), sink, self.pause.clone()));
                if let VoiceKind::Room { channel, .. } = self.kind {
                    let _ = ctl.send(&Ctl::VoiceJoin { channel });
                }
                for s in &subs {
                    let _ = ctl.send(&Ctl::Subscribe { track: *s });
                }
                if let Some(s) = &self.share {
                    let to_peer = matches!(self.kind, VoiceKind::Room { .. }) || self.peer_watching;
                    if to_peer {
                        s.track.add_remote(peer, conn);
                    }
                }
                self.report_at = Some(Instant::now() + MEDIA_TICK);
                self.send_call_state();
            }
            Transport::Sfu { sfu, epoch, short_id } => {
                let sfu = sfu.clone();
                let (short_id, epoch) = (*short_id, *epoch);
                self.set_ids(short_id, epoch);
                if let VoiceKind::Room { channel, .. } = self.kind {
                    sfu.set_local(LocalPeer {
                        channel,
                        audio_in: self.audio_in.clone(),
                        video_in: self.video_in.clone(),
                        subs: subs.iter().copied().collect(),
                    });
                }
                if let Some(s) = &self.share {
                    sfu.attach_local_track(s.track.clone());
                }
            }
        }
        // A new path: the share restarts from a fresh keyframe.
        if let Some(s) = &self.share
            && !matches!(self.transport, Transport::None)
        {
            self.emit(NetEvent::KeyframeRequested { track: s.track.track });
        }
        self.publish_info();
    }

    fn send_audio(&self, pkt: Bytes) {
        let Some(p) = stamp_audio(pkt, self.own_voice.load(Ordering::Relaxed), self.epoch) else { return };
        match &self.transport {
            Transport::Remote { conn, .. } => {
                let _ = conn.send_datagram(p);
            }
            Transport::Sfu { sfu, .. } => sfu.inject_local(p),
            Transport::None => {}
        }
    }

    fn start_share(&mut self, frames: mpsc::Receiver<VideoFrame>) {
        if let Some(s) = &mut self.share {
            s.frames = Some(frames);
            return;
        }
        let track = new_share_track(&self.net, &self.tasks, self.short_id);
        match &self.transport {
            Transport::Remote { conn, peer, .. } => {
                if matches!(self.kind, VoiceKind::Room { .. }) || self.peer_watching {
                    track.add_remote(*peer, conn.clone());
                }
            }
            Transport::Sfu { sfu, .. } => sfu.attach_local_track(track.clone()),
            Transport::None => {}
        }
        self.share = Some(Share { frames: Some(frames), track });
        self.set_sharing(true);
    }

    fn stop_share(&mut self) {
        let Some(s) = self.share.take() else { return };
        s.track.end();
        s.track.clear();
        if let Transport::Sfu { sfu, .. } = &self.transport {
            sfu.detach_local_track();
        }
        self.set_sharing(false);
    }

    fn set_sharing(&mut self, on: bool) {
        if let (VoiceKind::Room { channel, .. }, Some(net)) = (self.kind, self.net()) {
            net.set_status(|s| {
                if s.voice == Some(channel) {
                    s.sharing = on;
                }
            });
        }
        self.send_call_state();
        self.publish_info();
    }

    fn subscribe(&mut self, track: u16) {
        if track_kind(track) != KIND_SCREEN || !self.subs.lock().insert(track) {
            return;
        }
        match &self.transport {
            Transport::Remote { ctl, .. } => {
                let _ = ctl.send(&Ctl::Subscribe { track });
            }
            Transport::Sfu { sfu, .. } => sfu.local_subscribe(track),
            Transport::None => {}
        }
        self.publish_info();
    }

    fn unsubscribe(&mut self, track: u16) {
        if !self.subs.lock().remove(&track) {
            return;
        }
        self.stats.forget(track);
        self.video_in.forget(track);
        match &self.transport {
            Transport::Remote { ctl, .. } => {
                let _ = ctl.send(&Ctl::Unsubscribe { track });
            }
            Transport::Sfu { sfu, .. } => sfu.local_unsubscribe(track),
            Transport::None => {}
        }
        self.publish_info();
    }

    fn share_track_is(&self, track: u16) -> Option<&Arc<VideoTrack>> { self.share.as_ref().map(|s| &s.track).filter(|t| t.track == track) }

    fn on_peer_ctl(&mut self, ctl: Ctl) {
        let in_call = matches!(self.kind, VoiceKind::Call { .. });
        match ctl {
            Ctl::KeyframeRequest { track } => {
                let peer = match &self.transport {
                    Transport::Remote { peer, .. } => Some(*peer),
                    _ => None,
                };
                if let Some(t) = self.share_track_is(track) {
                    match (in_call, peer) {
                        // The peer is the viewer: restart it from the cache when possible.
                        (true, Some(p)) => t.viewer_wants_key(ViewerId::Peer(p)),
                        _ => t.request_keyframe(),
                    }
                }
            }
            Ctl::TargetBitrate { track, bps } => {
                if self.share_track_is(track).is_some() {
                    self.emit(NetEvent::TargetBitrate { track, bps });
                }
            }
            Ctl::ReceiverReport { track, rx, lost, late, jitter_ms } => {
                let r = super::stats::Report { rx, lost, late, jitter_ms };
                if track == self.own_voice.load(Ordering::Relaxed) {
                    let pct = r.loss_pct();
                    if self.last_loss != Some(pct) {
                        self.last_loss = Some(pct);
                        self.emit(NetEvent::LossReport { pct });
                    }
                } else if in_call
                    && lost > 0
                    && let Some(t) = self.share_track_is(track)
                {
                    t.report_loss();
                }
            }
            Ctl::SoundPlayed { by, clip } => {
                if by != self.me {
                    self.emit(NetEvent::SoundPlayed { conv: self.conv, by, clip });
                }
            }
            Ctl::Subscribe { track } if in_call => {
                if track == track_id(self.short_id, KIND_SCREEN) {
                    self.peer_watching = true;
                    if let (Transport::Remote { conn, peer, .. }, Some(s)) = (&self.transport, &self.share) {
                        s.track.add_remote(*peer, conn.clone());
                    }
                }
            }
            Ctl::Unsubscribe { track } if in_call => {
                if track == track_id(self.short_id, KIND_SCREEN) {
                    self.peer_watching = false;
                    if let (Transport::Remote { peer, .. }, Some(s)) = (&self.transport, &self.share) {
                        s.track.remove(ViewerId::Peer(*peer));
                    }
                }
            }
            other => tracing::trace!(?other, "voice: ignoring control message"),
        }
    }

    fn need_keyframe(&mut self, track: u16) {
        if !self.subs.lock().contains(&track) {
            return;
        }
        let now = Instant::now();
        if self.kf_asked.get(&track).is_some_and(|t| now.saturating_duration_since(*t) < KEYFRAME_EVERY) {
            return;
        }
        self.kf_asked.insert(track, now);
        if let Some(c) = self.ctl() {
            let _ = c.send(&Ctl::KeyframeRequest { track });
        }
    }

    fn sound(&self, clip: ClipInfo) {
        match &self.transport {
            Transport::Remote { ctl, .. } => {
                let _ = ctl.send(&Ctl::SoundPlayed { by: self.me, clip });
            }
            Transport::Sfu { sfu, .. } => sfu.local_sound(self.me, clip),
            Transport::None => {}
        }
    }

    fn send_call_state(&self) {
        if !matches!(self.kind, VoiceKind::Call { .. }) {
            return;
        }
        let (Some(c), Some(net)) = (self.ctl(), self.net()) else { return };
        let st = net.status(true);
        let sharing = self.share.as_ref().map(|s| s.track.track);
        let _ = c.send(&Ctl::CallState { call_id: self.call_id, muted: st.muted, deafened: st.deafened, sharing });
    }

    /// Every second while remote: receiver reports, and in a call the share's bitrate.
    fn report(&mut self) {
        let now = Instant::now();
        self.report_at = Some(now + MEDIA_TICK);
        let Transport::Remote { ctl, conn, .. } = &self.transport else {
            self.report_at = None;
            return;
        };
        for (track, r) in self.stats.drain() {
            let _ = ctl.send(&Ctl::ReceiverReport { track, rx: r.rx, lost: r.lost, late: r.late, jitter_ms: r.jitter_ms });
        }
        let relayed = crate::addr::path_snapshot(conn).relay;
        if matches!(self.kind, VoiceKind::Call { .. })
            && let Some(s) = &self.share
        {
            let cap = if relayed { RELAYED_VIEWER_BPS } else { VIDEO_MAX_BPS };
            if let Some(bps) = s.track.tick(now, cap) {
                self.emit(NetEvent::TargetBitrate { track: s.track.track, bps });
            }
        }
        if self.info.lock().relayed != relayed {
            self.publish_info();
        }
    }

    /// Leave for good: tell the host, clear the status.
    fn leave(&mut self) {
        if let Some(s) = self.share.take() {
            s.track.end();
            s.track.clear();
        }
        if let (VoiceKind::Room { .. }, Some(c)) = (self.kind, self.ctl()) {
            for t in self.subs.lock().iter() {
                let _ = c.send(&Ctl::Unsubscribe { track: *t });
            }
            let _ = c.send(&Ctl::VoiceLeave);
        }
        self.teardown();
        if let (VoiceKind::Room { channel, .. }, Some(net)) = (self.kind, self.net()) {
            net.set_status(|s| {
                if s.voice == Some(channel) {
                    s.voice = None;
                    s.sharing = false;
                }
            });
        }
        let mut info = self.info.lock();
        info.connected = false;
        info.sharing = None;
    }

    /// Refresh the shared info; tell the app if it changed.
    fn publish_info(&self) {
        let (connected, relayed) = match &self.transport {
            Transport::None => (false, false),
            Transport::Remote { conn, .. } => (true, crate::addr::path_snapshot(conn).relay),
            Transport::Sfu { .. } => (true, false),
        };
        let mut watching: Vec<u16> = self.subs.lock().iter().copied().collect();
        watching.sort_unstable();
        let changed = {
            let mut info = self.info.lock();
            let next = VoiceInfo {
                conv: self.conv,
                channel: info.channel,
                peer: info.peer,
                local_track: self.own_voice.load(Ordering::Relaxed),
                epoch: self.epoch,
                connected,
                relayed,
                sharing: self.share.as_ref().map(|s| s.track.track),
                watching,
            };
            let changed = *info != next;
            *info = next;
            changed
        };
        if changed {
            self.emit(NetEvent::VoiceUpdated { conv: self.conv });
        }
    }
}

/// A share track whose keyframe requests go to the app.
fn new_share_track(net: &Weak<Inner>, tasks: &Arc<Tasks>, short_id: u16) -> Arc<VideoTrack> {
    let net = net.clone();
    VideoTrack::new(
        track_id(short_id, KIND_SCREEN),
        tasks.clone(),
        Box::new(move |track| {
            if let Some(n) = net.upgrade() {
                n.emit(NetEvent::KeyframeRequested { track });
            }
        }),
    )
}

/// Video received by a member or call session: subscribed tracks go to `video_in`.
struct SessionSink {
    video_in: VideoOut,
    subs: Arc<Mutex<HashSet<u16>>>,
    stats: Arc<RxStats>,
    cmd: mpsc::UnboundedSender<VoiceCmd>,
}

impl VideoSink for SessionSink {
    fn accept(&self, track: u16) -> bool { track_kind(track) == KIND_SCREEN }

    fn frame(&self, track: u16, f: VideoFrame) -> bool {
        if !self.subs.lock().contains(&track) {
            return true;
        }
        let ts = f.ts;
        match self.video_in.send(track, f) {
            Ok(()) => {
                self.stats.with(track, |t| t.video(ts, Instant::now()));
                true
            }
            Err(OutError::Full | OutError::Closed) => {
                self.stats.with(track, |t| t.late());
                false
            }
        }
    }

    fn gap(&self, track: u16) {
        self.stats.with(track, |t| t.lost(1));
        let _ = self.cmd.send(VoiceCmd::NeedKeyframe(track));
    }
}
