//! Small media helpers: encoded frames shared by viewers, audio header stamping, control
//! frames straight onto a link's live queue.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::Mutex;
use tincan_core::media::{AUDIO_HEADER_LEN, AUDIO_MAGIC, KIND_SHARE_AUDIO, KIND_VOICE, VideoFrame, encode_video_frame, track_kind, track_short_id};
use tincan_core::wire::Ctl;
use tokio::sync::mpsc;

/// A video frame encoded for a GOP stream once and shared (cheap clones) by every viewer and
/// the GOP cache.
#[derive(Clone, Debug)]
pub(crate) struct Wire {
    /// `varint len | ts | fflags | access unit`.
    pub bytes: Bytes,
    data_off: usize,
    /// 90 kHz timestamp.
    pub ts: u32,
    /// Keyframe.
    pub key: bool,
}

impl Wire {
    /// Encode `f`.
    pub fn encode(f: &VideoFrame) -> Wire {
        let bytes = encode_video_frame(f);
        let data_off = bytes.len().saturating_sub(f.data.len());
        Wire { bytes, data_off, ts: f.ts, key: f.keyframe }
    }

    /// The frame again (no copy).
    pub fn frame(&self) -> VideoFrame { VideoFrame { ts: self.ts, keyframe: self.key, data: self.bytes.slice(self.data_off..) } }

    /// Encoded size.
    pub fn len(&self) -> usize { self.bytes.len() }
}

/// Write the current track id and epoch byte into an outgoing audio datagram (copies only when
/// they differ). None for anything that is not an audio datagram.
pub(crate) fn stamp_audio(pkt: Bytes, track: u16, epoch: u8) -> Option<Bytes> {
    if pkt.len() < AUDIO_HEADER_LEN || pkt[0] != AUDIO_MAGIC {
        return None;
    }
    let t = track.to_be_bytes();
    if pkt[2] == t[0] && pkt[3] == t[1] && pkt[11] == epoch {
        return Some(pkt);
    }
    let mut v = pkt.to_vec();
    v[2..4].copy_from_slice(&t);
    v[11] = epoch;
    Some(Bytes::from(v))
}

/// The voice track of an audio datagram sent by the member with `short_id`; None if the
/// datagram is malformed or claims someone else's track.
pub(crate) fn voice_track_from(pkt: &[u8], short_id: u16) -> Option<u16> {
    let track = tincan_core::media::audio_track_of(pkt)?;
    (track_kind(track) == KIND_VOICE && track_short_id(track) == short_id).then_some(track)
}

/// The share-audio track of an audio datagram sent by the member with `short_id`; None if it
/// is malformed, voice, or claims someone else's share.
pub(crate) fn share_audio_track_from(pkt: &[u8], short_id: u16) -> Option<u16> {
    let track = tincan_core::media::audio_track_of(pkt)?;
    (track_kind(track) == KIND_SHARE_AUDIO && track_short_id(track) == short_id).then_some(track)
}

/// A group replayed from a cache after a migration may start up to this far (90 kHz, 10 s)
/// behind what the app already has; anything further back is a restarted share.
const REPLAY_WINDOW: i32 = 10 * 90_000;

/// Why a frame was not handed to the app.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OutError {
    /// The app is not keeping up.
    Full,
    /// The session ended.
    Closed,
}

/// Per-track state of [`VideoOut`].
#[derive(Clone, Copy, Debug)]
struct Handed {
    /// Timestamp of the last frame handed to the app.
    last: u32,
    /// A new path started: drop frames up to `last` until the first newer one.
    armed: bool,
}

/// The app's `video_in`, shared by every path of one session (member downlink, host SFU,
/// call), with the last timestamp handed over per track. After a transport change
/// ([`VideoOut::arm`]) a frame at or before that (within `REPLAY_WINDOW`) is dropped
/// quietly until the first newer one, so a group replayed from a GOP cache by a new host does
/// not rewind the picture: the stream continues after the last frame the app has. Outside
/// that window nothing is filtered, so a share that restarts its timeline is delivered.
#[derive(Clone, Debug)]
pub(crate) struct VideoOut {
    tx: mpsc::Sender<(u16, VideoFrame)>,
    st: Arc<Mutex<HashMap<u16, Handed>>>,
}

impl VideoOut {
    /// Wrap the session's `video_in` sender.
    pub fn new(tx: mpsc::Sender<(u16, VideoFrame)>) -> Self { Self { tx, st: Arc::new(Mutex::new(HashMap::new())) } }

    /// A new transport starts: hold back replays of what the app already has.
    pub fn arm(&self) {
        for h in self.st.lock().values_mut() {
            h.armed = true;
        }
    }

    /// Hand `f` to the app (Ok also when it was a replay and was dropped).
    pub fn send(&self, track: u16, f: VideoFrame) -> Result<(), OutError> {
        let mut st = self.st.lock();
        if let Some(h) = st.get_mut(&track)
            && h.armed
        {
            let d = f.ts.wrapping_sub(h.last) as i32;
            if d <= 0 && d > -REPLAY_WINDOW {
                return Ok(());
            }
            h.armed = false;
        }
        let ts = f.ts;
        match self.tx.try_send((track, f)) {
            Ok(()) => {
                st.insert(track, Handed { last: ts, armed: false });
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => Err(OutError::Full),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(OutError::Closed),
        }
    }

    /// Forget a track (unsubscribed).
    pub fn forget(&self, track: u16) { self.st.lock().remove(&track); }
}

/// Control frames for one peer, queued directly on its control link's live queue (same order
/// as the session's own frames; never blocks; dropped when the queue is full or gone).
#[derive(Clone, Debug)]
pub(crate) struct CtlTx(pub mpsc::Sender<Bytes>);

impl CtlTx {
    /// Queue `ctl`. False if it could not be queued.
    pub fn send(&self, ctl: &Ctl) -> bool {
        match crate::link::encode(ctl) {
            Some(b) => self.0.try_send(b).is_ok(),
            None => false,
        }
    }

    /// Queue an ephemeral frame relayed from someone else (`SoundPlayed`): only while the
    /// queue is less than half full, so relayed chatter never crowds out session frames.
    pub fn send_relayed(&self, ctl: &Ctl) -> bool { self.0.capacity() > crate::link::LIVE_QUEUE / 2 && self.send(ctl) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tincan_core::media::{AF_SPEAKING, AudioHeader, KIND_SCREEN, track_id};

    #[test]
    fn wire_slices_the_frame_back_out() {
        let f = VideoFrame { ts: 9000, keyframe: true, data: Bytes::from(vec![0, 0, 0, 1, 0x65, 7, 7]) };
        let w = Wire::encode(&f);
        assert_eq!(w.frame(), f);
        assert!(w.key && w.ts == 9000);
        assert_eq!(w.len(), encode_video_frame(&f).len());
        let empty = VideoFrame { ts: 1, keyframe: false, data: Bytes::new() };
        assert_eq!(Wire::encode(&empty).frame(), empty);
    }

    #[test]
    fn stamping_rewrites_track_and_epoch_only() {
        let h = AudioHeader { flags: AF_SPEAKING, track: 4, seq: 77, ts: 123_456, level: 30, epoch: 1 };
        let pkt = h.packet(b"opus");
        // Same values: the very same buffer comes back.
        let same = stamp_audio(pkt.clone(), 4, 1).unwrap();
        assert_eq!(same.as_ptr(), pkt.as_ptr());
        let moved = stamp_audio(pkt, 12, 9).unwrap();
        let (back, payload) = AudioHeader::parse(&moved).unwrap();
        assert_eq!(back, AudioHeader { track: 12, epoch: 9, ..h });
        assert_eq!(payload, b"opus");
        assert!(stamp_audio(Bytes::from_static(b"short"), 1, 1).is_none());
    }

    #[test]
    fn replays_are_not_delivered_twice() {
        let (tx, mut rx) = mpsc::channel(8);
        let out = VideoOut::new(tx);
        let f = |ts: u32, key: bool| VideoFrame { ts, keyframe: key, data: Bytes::new() };
        for ts in [0u32, 3000, 6000] {
            out.send(7, f(ts, ts == 0)).unwrap();
        }
        // The cached group comes again over a new path: only what is new gets through.
        out.arm();
        for ts in [0u32, 3000, 6000, 9000] {
            out.send(7, f(ts, ts == 0)).unwrap();
        }
        let got: Vec<u32> = std::iter::from_fn(|| rx.try_recv().ok()).map(|(_, f)| f.ts).collect();
        assert_eq!(got, vec![0, 3000, 6000, 9000]);
        // Disarmed by the first new frame: a share that restarts its timeline is delivered.
        out.send(7, f(0, true)).unwrap();
        out.send(7, f(3000, false)).unwrap();
        assert_eq!(std::iter::from_fn(|| rx.try_recv().ok()).count(), 2);
        // Armed again, a restart far behind (outside the replay window) is new too.
        out.arm();
        out.send(7, f(3000u32.wrapping_sub(2_000_000), true)).unwrap();
        assert!(rx.try_recv().is_ok());
        // Other tracks are independent; forgetting starts over.
        out.send(9, f(0, true)).unwrap();
        out.forget(7);
        out.send(7, f(0, true)).unwrap();
        assert_eq!(std::iter::from_fn(|| rx.try_recv().ok()).count(), 2);
        // Full and closed are reported.
        for _ in 0..8 {
            let _ = out.send(5, f(rand_ts(), false));
        }
        assert_eq!(out.send(5, f(u32::MAX / 2, false)), Err(OutError::Full));
        drop(rx);
        assert_eq!(out.send(6, f(1, true)), Err(OutError::Closed));
    }

    fn rand_ts() -> u32 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(1);
        N.fetch_add(3000, Ordering::Relaxed)
    }

    #[test]
    fn senders_cannot_claim_other_tracks() {
        let pkt = |track| AudioHeader { track, ..AudioHeader::default() }.packet(b"x");
        assert_eq!(voice_track_from(&pkt(track_id(3, KIND_VOICE)), 3), Some(12));
        assert_eq!(voice_track_from(&pkt(track_id(4, KIND_VOICE)), 3), None);
        assert_eq!(voice_track_from(&pkt(track_id(3, KIND_SCREEN)), 3), None);
        // Share audio: a voice check refuses it (what builds without share audio do), the
        // share check takes only the sender's own share-audio track.
        assert_eq!(voice_track_from(&pkt(track_id(3, KIND_SHARE_AUDIO)), 3), None);
        assert_eq!(share_audio_track_from(&pkt(track_id(3, KIND_SHARE_AUDIO)), 3), Some(track_id(3, KIND_SHARE_AUDIO)));
        assert_eq!(share_audio_track_from(&pkt(track_id(4, KIND_SHARE_AUDIO)), 3), None);
        assert_eq!(share_audio_track_from(&pkt(track_id(3, KIND_VOICE)), 3), None);
        assert_eq!(share_audio_track_from(&[0xA1, 0, 0], 3), None);
        assert_eq!(voice_track_from(b"\xA1", 3), None);
    }
}
