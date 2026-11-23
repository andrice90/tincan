//! Screen share: sources, sharing and watching.
//!
//! A share is a `tincan_screen::ScreenShare` feeding the frame channel from
//! `Net::share_start`. With `TestOptions::no_devices` nothing is captured; instead the
//! source [`TEST_SOURCE_ID`] produces a synthetic Annex-B pattern through the same path, so
//! the app's wiring (keyframe requests, bitrate, watching, ending) runs in tests without a
//! GPU. `ScreenShare::stop` and its `Drop` join the capture thread, so every share is
//! stopped on a blocking thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tincan_core::media::VideoFrame;
use tincan_screen::{EndReason, ScreenShare, ShareMode};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::task::AbortHandle;

use super::Media;
use super::session::{SHARE_UPLOAD_LIMIT, VOICE_UPLOAD_LIMIT};
use crate::app::{Core, net_err};
use crate::dto::{self, ToastKind};
use crate::VideoSink;

/// The synthetic source offered with `TestOptions::no_devices`.
pub(crate) const TEST_SOURCE_ID: &str = "test:pattern";
/// Synthetic frames per second.
const TEST_FPS: u64 = 15;
/// A synthetic keyframe every this many frames (4 s, like the real GOP).
const TEST_GOP: u32 = 60;

/// My running share.
pub(super) struct ActiveShare {
    /// Distinguishes shares (a late end notice must not stop a newer one).
    pub id: u64,
    imp: ShareImpl,
}

enum ShareImpl {
    Screen(ScreenShare),
    Synthetic { task: AbortHandle, keyframe: Arc<AtomicBool> },
}

impl ActiveShare {
    /// A viewer needs a keyframe.
    pub(super) fn request_keyframe(&self) {
        match &self.imp {
            ShareImpl::Screen(s) => s.request_keyframe(),
            ShareImpl::Synthetic { keyframe, .. } => keyframe.store(true, Ordering::Release),
        }
    }

    /// New bitrate target from the forwarding hop.
    pub(super) fn set_bitrate(&self, bps: u32) {
        if let ShareImpl::Screen(s) = &self.imp {
            s.set_bitrate(bps);
        }
    }

    /// Stop and wait for the capture thread to finish (on a blocking thread).
    pub(super) async fn stop(self) {
        match self.imp {
            ShareImpl::Screen(s) => {
                let _ = tokio::task::spawn_blocking(move || s.stop()).await;
            }
            ShareImpl::Synthetic { task, .. } => task.abort(),
        }
    }

    /// Stop without waiting.
    pub(super) fn stop_in_background(self) {
        match self.imp {
            ShareImpl::Screen(s) => {
                tokio::task::spawn_blocking(move || s.stop());
            }
            ShareImpl::Synthetic { task, .. } => task.abort(),
        }
    }
}

/// One synthetic access unit: AUD, SPS, PPS and IDR for a keyframe, else one slice.
fn synthetic_au(n: u32, key: bool) -> Bytes {
    let mut au = vec![0, 0, 0, 1, 0x09, 0x10];
    if key {
        au.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0xda, 0x01, 0x40, 0x16, 0xe8]);
        au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
        au.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88]);
    } else {
        au.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9a]);
    }
    au.extend_from_slice(&n.to_be_bytes());
    au.resize(au.len() + if key { 2000 } else { 400 }, 0x5a);
    Bytes::from(au)
}

/// The synthetic pattern: `TEST_FPS` frames per second until the channel closes.
fn spawn_synthetic(frames: mpsc::Sender<VideoFrame>, keyframe: Arc<AtomicBool>) -> AbortHandle {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(1000 / TEST_FPS));
        let mut n = 0u32;
        let mut force = true;
        loop {
            tick.tick().await;
            let key = force || keyframe.swap(false, Ordering::AcqRel) || n.is_multiple_of(TEST_GOP);
            let f = VideoFrame { ts: n.wrapping_mul((90_000 / TEST_FPS) as u32), keyframe: key, data: synthetic_au(n, key) };
            match frames.try_send(f) {
                Ok(()) => force = false,
                // Like tincan-screen: drop, then restart at a keyframe.
                Err(TrySendError::Full(_)) => force = true,
                Err(TrySendError::Closed(_)) => return,
            }
            n = n.wrapping_add(1);
        }
    })
    .abort_handle()
}

/// User-facing text for a share that could not start.
fn start_err(e: &tincan_screen::Error) -> String {
    if let Some(code) = e.code() {
        return code.to_string();
    }
    match e {
        tincan_screen::Error::InvalidSource(_) | tincan_screen::Error::SourceUnavailable(_) => {
            "That screen or window isn't available. It may be closed or minimized. Pick another one.".to_string()
        }
        tincan_screen::Error::PermissionDenied(_) => "Windows didn't allow capturing that window. Pick another one.".to_string(),
        tincan_screen::Error::Unsupported(_) => "Screen sharing needs Windows 10 version 2004 or newer.".to_string(),
        other => {
            tracing::warn!(error = %other, "starting a screen share failed");
            "Couldn't start screen sharing. Try again.".to_string()
        }
    }
}

impl Media {
    /// Real capture is allowed.
    fn capture_allowed(&self) -> bool { !self.no_devices || self.allow_capture.load(Ordering::Acquire) }

    /// Is there a hardware H.264 encoder (asked once, on a blocking thread)?
    async fn hw_available(&self) -> bool {
        *self
            .hw_encoder
            .get_or_init(|| async { tokio::task::spawn_blocking(tincan_screen::hardware_encoder_available).await.unwrap_or(false) })
            .await
    }

    /// `screen_sources`.
    pub(super) async fn screen_sources(&self) -> Result<Vec<dto::ScreenSource>, String> {
        let mut out = Vec::new();
        if self.capture_allowed() {
            let list = tincan_screen::sources().await.map_err(|e| {
                tracing::warn!(error = %e, "listing screen sources failed");
                "Couldn't list your screens and windows. Try again.".to_string()
            })?;
            out.extend(list.into_iter().map(|s| dto::ScreenSource {
                id: s.id,
                kind: match s.kind {
                    tincan_screen::SourceKind::Display => dto::ScreenSourceKind::Display,
                    tincan_screen::SourceKind::Window => dto::ScreenSourceKind::Window,
                },
                name: s.name,
                width: s.width,
                height: s.height,
            }));
        }
        if self.no_devices {
            out.push(dto::ScreenSource {
                id: TEST_SOURCE_ID.into(),
                kind: dto::ScreenSourceKind::Display,
                name: "Test pattern".into(),
                width: 1280,
                height: 720,
            });
        }
        Ok(out)
    }

    /// `screen_start`: returns my screen track id.
    pub(super) async fn screen_start(&self, core: &Arc<Core>, source_id: &str, mode: dto::ShareMode) -> Result<u16, String> {
        let _ops = self.ops.lock().await;
        if self.st.lock().attached.is_none() {
            return Err("not-in-voice".into());
        }
        let synthetic = self.no_devices && source_id == TEST_SOURCE_ID;
        if !synthetic {
            if !self.capture_allowed() {
                return Err("Screen capture is off in this test profile.".into());
            }
            if !self.hw_available().await {
                return Err("no-hardware-encoder".into());
            }
        }
        let old = self.st.lock().share.take();
        if let Some(old) = old {
            core.net.share_stop();
            old.stop().await;
        }
        let (track, frames) = core.net.share_start().map_err(net_err)?;
        let imp = if synthetic {
            let keyframe = Arc::new(AtomicBool::new(false));
            ShareImpl::Synthetic { task: spawn_synthetic(frames, keyframe.clone()), keyframe }
        } else {
            let mode = match mode {
                dto::ShareMode::Text => ShareMode::Text,
                dto::ShareMode::Motion => ShareMode::Motion,
            };
            match ScreenShare::start(source_id, mode, frames).await {
                Ok(s) => ShareImpl::Screen(s),
                Err(e) => {
                    core.net.share_stop();
                    return Err(start_err(&e));
                }
            }
        };
        let id = self.next_id();
        let ended = match &imp {
            ShareImpl::Screen(s) => Some(s.ended()),
            ShareImpl::Synthetic { .. } => None,
        };
        let share = ActiveShare { id, imp };
        let refused = {
            let mut st = self.st.lock();
            if st.attached.is_none() {
                // Left voice while capture was starting.
                Some(share)
            } else {
                st.share = Some(share);
                core.files.set_upload_limit(Some(SHARE_UPLOAD_LIMIT));
                None
            }
        };
        if let Some(s) = refused {
            core.net.share_stop();
            s.stop().await;
            return Err("not-in-voice".into());
        }
        if let Some(mut ended) = ended {
            let weak = Arc::downgrade(core);
            tokio::spawn(async move {
                let reason = match ended.wait_for(Option::is_some).await {
                    Ok(r) => r.clone(),
                    // The share thread went away without a reason: it panicked.
                    Err(_) => Some(EndReason::Failed("the capture thread stopped".into())),
                };
                if let Some(core) = weak.upgrade() {
                    core.media.on_share_ended(&core, id, reason);
                }
            });
        }
        tracing::info!(track, synthetic, "screen share started");
        self.reconcile(core);
        Ok(track)
    }

    /// A share ended by itself (window closed, capture failed).
    fn on_share_ended(&self, core: &Arc<Core>, id: u64, reason: Option<EndReason>) {
        let share = {
            let mut st = self.st.lock();
            if !st.share.as_ref().is_some_and(|s| s.id == id) {
                // Stopped by us, or replaced.
                return;
            }
            if st.attached.is_some() {
                core.files.set_upload_limit(Some(VOICE_UPLOAD_LIMIT));
            }
            st.share.take()
        };
        core.net.share_stop();
        if let Some(s) = share {
            s.stop_in_background();
        }
        match reason {
            Some(EndReason::SourceClosed(_)) => core.toast(ToastKind::Info, "The shared window was closed, so screen sharing stopped."),
            Some(EndReason::Failed(msg)) => {
                tracing::warn!(reason = %msg, "screen share failed");
                core.toast(ToastKind::Error, "Screen sharing stopped because capture failed. Try sharing again.");
            }
            _ => {}
        }
        self.reconcile(core);
    }

    /// `screen_stop`.
    pub(super) async fn screen_stop(&self, core: &Arc<Core>) {
        let _ops = self.ops.lock().await;
        let share = {
            let mut st = self.st.lock();
            if st.attached.is_some() {
                core.files.set_upload_limit(Some(VOICE_UPLOAD_LIMIT));
            }
            st.share.take()
        };
        core.net.share_stop();
        if let Some(s) = share {
            s.stop().await;
        }
        self.reconcile(core);
    }

    /// `App::screen_watch`: deliver frames of `track` to `sink` (replacing an older sink).
    pub(super) fn watch(&self, core: &Arc<Core>, track: u16, sink: VideoSink) -> Result<(), String> {
        let replaced = {
            let st = self.st.lock();
            if st.attached.is_none() {
                return Err("not-in-voice".into());
            }
            if let Some(live) = self.live_tracks(core, &st)
                && !live.contains(&track)
            {
                return Err("That screen share has ended.".into());
            }
            self.watchers.lock().insert(track, sink).is_some()
        };
        if replaced {
            // Resubscribe so the new sink starts at a keyframe (the host seeds from its cache).
            core.net.unsubscribe_track(track);
        }
        core.net.subscribe_track(track);
        Ok(())
    }

    /// `screen_unwatch`.
    pub(super) fn screen_unwatch(&self, core: &Arc<Core>, track: u16) {
        self.watchers.lock().remove(&track);
        core.net.unsubscribe_track(track);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tincan_core::media::{h264_codec_string, h264_is_keyframe};

    #[test]
    fn synthetic_frames_look_like_annex_b() {
        let k = synthetic_au(3, true);
        assert!(h264_is_keyframe(&k));
        assert_eq!(h264_codec_string(&k).as_deref(), Some("avc1.42c01f"));
        assert!(!h264_is_keyframe(&synthetic_au(4, false)));
    }

    #[test]
    fn errors_map_to_api_strings() {
        assert_eq!(start_err(&tincan_screen::Error::NoHardwareEncoder), "no-hardware-encoder");
        assert!(start_err(&tincan_screen::Error::SourceUnavailable("x".into())).contains("Pick another one"));
    }
}
