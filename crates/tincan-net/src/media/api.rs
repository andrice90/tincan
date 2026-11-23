//! Public voice, screen-share and call API on [`Net`].

use tincan_core::media::VideoFrame;
use tincan_core::wire::ClipInfo;
use tincan_core::{ChannelId, ServerId, UserId};
use tokio::sync::mpsc;

use super::{CallInfo, MediaSession, VoiceInfo};
use crate::error::Result;
use crate::net::Net;

impl Net {
    /// Join a voice room of `server` (leaving any other room; an active or outgoing call is
    /// hung up, an incoming one keeps ringing). Sets my `Status.voice` for that server.
    ///
    /// Returns at once; media flows as soon as the server has a host (`NetEvent::VoiceUpdated`,
    /// `voice_info().connected`). The session survives host migrations with the same channels.
    pub async fn voice_join(&self, server: ServerId, channel: ChannelId) -> Result<MediaSession> {
        let inner = self.inner();
        inner.media.voice_join(inner, server, channel).await
    }

    /// Leave the voice room, or hang up the active call.
    pub async fn voice_leave(&self) {
        let inner = self.inner();
        inner.media.voice_leave(inner).await;
    }

    /// My current voice session (an active call wins over a room).
    pub fn voice_info(&self) -> Option<VoiceInfo> { self.inner().media.voice_info() }

    /// Start sharing my screen in the current voice session. Returns my screen track id and
    /// the channel the encoder feeds (`tincan_screen::ScreenShare::start(.., frames)`).
    /// Keyframe requests and bitrate targets for it arrive as `NetEvent::KeyframeRequested`
    /// and `NetEvent::TargetBitrate`. Dropping the sender ends the share.
    /// Errors with `NetError::NotInVoice` outside a voice session.
    pub fn share_start(&self) -> Result<(u16, mpsc::Sender<VideoFrame>)> { self.inner().media.share_start() }

    /// Stop sharing.
    pub fn share_stop(&self) { self.inner().media.share_stop(); }

    /// Watch a screen-share track (from the roster's `sharing`, or a call peer's
    /// `CallInfo::peer_sharing`). Frames arrive on the session's `video_in`, starting at a
    /// keyframe (the host's cached group when there is one).
    pub fn subscribe_track(&self, track: u16) { self.inner().media.subscribe(track); }

    /// Stop watching a track.
    pub fn unsubscribe_track(&self, track: u16) { self.inner().media.unsubscribe(track); }

    /// Tell the others in my room (or call) that I played `clip`; they get
    /// `NetEvent::SoundPlayed` and can fetch it from me. Errors with `NetError::NotInVoice`
    /// outside a voice session.
    pub async fn sound_played(&self, clip: ClipInfo) -> Result<()> { self.inner().media.sound(clip) }

    /// Call `user` over the DM connection (dialing it if needed). Returns the ringing call and
    /// its media session; media flows once they accept (`NetEvent::CallUpdated` with state
    /// `Active`). Ringing ends after 45 s. Errors if a call already exists or they cannot be
    /// reached.
    pub async fn call_start(&self, user: UserId) -> Result<(CallInfo, MediaSession)> {
        let inner = self.inner();
        inner.media.call_start(inner, user).await
    }

    /// Answer the ringing call `call_id` (leaves the voice room).
    pub async fn call_accept(&self, call_id: u64) -> Result<MediaSession> {
        let inner = self.inner();
        inner.media.call_accept(inner, call_id)
    }

    /// Decline the ringing call `call_id`.
    pub async fn call_decline(&self, call_id: u64) {
        let inner = self.inner();
        inner.media.call_decline(inner, call_id);
    }

    /// Hang up the current call (cancels an outgoing ring, declines an incoming one).
    pub async fn call_hangup(&self) {
        let inner = self.inner();
        inner.media.hangup(inner);
    }

    /// The current call, if any, with the current RTT of its DM connection.
    pub fn call_info(&self) -> Option<CallInfo> { self.inner().media.call_info_with_rtt(self.inner()) }

    /// Test hook: stop (or resume) reading incoming video streams, like a viewer that cannot
    /// keep up.
    #[cfg(test)]
    pub(crate) fn pause_video(&self, paused: bool) { self.inner().media.set_paused(paused); }
}
