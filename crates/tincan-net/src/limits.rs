//! Rate limits and policers for what peers send: token buckets keyed per peer, the audio
//! datagram policer and the video ingress policer of the host SFU.
//!
//! Every limit here sits well above what an honest client sends; it only bounds what a
//! modified client can make this device (or, through it, everyone else) do.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::Duration;

use tokio::time::Instant;

/// A token bucket: `burst` tokens, refilled at `per_sec` tokens per second.
#[derive(Clone, Debug)]
pub(crate) struct Bucket {
    tokens: f64,
    last: Instant,
    per_sec: f64,
    burst: f64,
}

impl Bucket {
    /// A full bucket.
    pub fn new(per_sec: f64, burst: f64, now: Instant) -> Self { Self { tokens: burst, last: now, per_sec, burst } }

    /// One token per `every`, `burst` at most.
    pub fn every(every: Duration, burst: u32, now: Instant) -> Self {
        let per_sec = if every.is_zero() { f64::INFINITY } else { 1.0 / every.as_secs_f64() };
        Self::new(per_sec, f64::from(burst), now)
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * self.per_sec).min(self.burst);
    }

    /// Take `n` tokens if there are that many.
    pub fn take_n(&mut self, n: f64, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }

    /// Take one token if there is one.
    pub fn take(&mut self, now: Instant) -> bool { self.take_n(1.0, now) }

    /// Would one token be available now (without taking it)?
    pub fn ready(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.tokens >= 1.0
    }
}

/// Buckets per key (a user, a (user, channel) pair, an invite), all with the same rate.
/// Bounded: when full, keys whose bucket is full again (idle) are forgotten first.
#[derive(Debug)]
pub(crate) struct Buckets<K> {
    map: HashMap<K, Bucket>,
    per_sec: f64,
    burst: f64,
}

/// Most keys a `Buckets` keeps.
const MAX_KEYS: usize = 4096;

impl<K: Eq + Hash + Clone> Buckets<K> {
    /// One token per `every` and key, `burst` at most.
    pub fn new(every: Duration, burst: u32) -> Self {
        let per_sec = if every.is_zero() { f64::INFINITY } else { 1.0 / every.as_secs_f64() };
        Self::rate(per_sec, f64::from(burst))
    }

    /// `per_sec` tokens per second and key, `burst` at most (for byte budgets).
    pub fn rate(per_sec: f64, burst: f64) -> Self { Self { map: HashMap::new(), per_sec, burst } }

    /// Take a token for `key`.
    pub fn take(&mut self, key: &K, now: Instant) -> bool { self.take_n(key, 1.0, now) }

    /// Take `n` tokens for `key` if it has that many.
    pub fn take_n(&mut self, key: &K, n: f64, now: Instant) -> bool {
        if !self.map.contains_key(key) && self.map.len() >= MAX_KEYS {
            let burst = self.burst;
            self.map.retain(|_, b| {
                b.refill(now);
                b.tokens < burst
            });
            if self.map.len() >= MAX_KEYS {
                return false;
            }
        }
        let (per_sec, burst) = (self.per_sec, self.burst);
        self.map.entry(key.clone()).or_insert_with(|| Bucket::new(per_sec, burst, now)).take_n(n, now)
    }

    /// Would `take(key)` succeed now (without taking a token)? A key not seen yet has a full
    /// bucket.
    pub fn ready(&mut self, key: &K, now: Instant) -> bool { self.map.get_mut(key).is_none_or(|b| b.ready(now)) }
}

/// Largest Opus packet (RFC 6716).
pub(crate) const OPUS_MAX_PACKET: usize = 1275;

/// Per-member audio datagram policer at the SFU: packets and bytes per second, and a size
/// cap. Honest voice is 50 packets/s of at most a few hundred bytes.
#[derive(Debug)]
pub(crate) struct AudioPolicer {
    packets: Bucket,
    bytes: Bucket,
    /// Datagrams dropped so far.
    pub dropped: u64,
}

/// Packets per second allowed per member. Opus at 20 ms is 50/s per stream, and a member
/// playing a clip sends two streams (voice and soundboard): 100/s, with room for jitter clumps.
pub(crate) const AUDIO_PPS: f64 = 150.0;
/// Packet burst.
pub(crate) const AUDIO_BURST: f64 = 75.0;
/// Bytes per second allowed per member (512 kbit/s; honest voice is at most about 60).
pub(crate) const AUDIO_BYTES_PER_S: f64 = 64.0 * 1024.0;

impl AudioPolicer {
    /// A fresh policer.
    pub fn new(now: Instant) -> Self {
        Self { packets: Bucket::new(AUDIO_PPS, AUDIO_BURST, now), bytes: Bucket::new(AUDIO_BYTES_PER_S, AUDIO_BYTES_PER_S / 2.0, now), dropped: 0 }
    }

    /// May a datagram of `len` bytes be forwarded?
    pub fn allow(&mut self, len: usize, now: Instant) -> bool {
        let ok = len <= tincan_core::media::AUDIO_HEADER_LEN + OPUS_MAX_PACKET && self.packets.ready(now) && self.bytes.take_n(len as f64, now) && self.packets.take(now);
        if !ok {
            self.dropped += 1;
        }
        ok
    }
}

/// Per-track video ingress policer at the SFU: bytes and frames per second. Over budget
/// the frame is dropped and the track waits for the next keyframe that fits.
#[derive(Debug)]
pub(crate) struct VideoPolicer {
    bytes: Bucket,
    frames: Bucket,
    dropping: bool,
}

/// Video ingress cap in bits per second (twice the largest honest target).
pub(crate) const VIDEO_INGRESS_BPS: u32 = 2 * crate::media::VIDEO_MAX_BPS;
/// Video ingress burst in bytes (one large keyframe).
pub(crate) const VIDEO_INGRESS_BURST: f64 = 2.0 * 1024.0 * 1024.0;
/// Frames per second allowed per track.
pub(crate) const VIDEO_MAX_FPS: f64 = 60.0;

/// What to do with an uploaded frame.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum VideoVerdict {
    /// Forward it.
    Pass,
    /// Drop it; the first drop of a run is a gap (viewers skip to the next keyframe).
    Drop {
        /// This drop starts a run.
        gap: bool,
    },
}

impl VideoPolicer {
    /// A fresh policer.
    pub fn new(now: Instant) -> Self {
        Self {
            bytes: Bucket::new(f64::from(VIDEO_INGRESS_BPS) / 8.0, VIDEO_INGRESS_BURST, now),
            frames: Bucket::new(VIDEO_MAX_FPS, VIDEO_MAX_FPS, now),
            dropping: false,
        }
    }

    /// Police one frame of `len` bytes.
    pub fn check(&mut self, len: usize, keyframe: bool, now: Instant) -> VideoVerdict {
        if self.dropping && !keyframe {
            // Deltas after a drop are useless until the next keyframe.
            return VideoVerdict::Drop { gap: false };
        }
        let ok = self.frames.ready(now) && self.bytes.take_n(len as f64, now) && self.frames.take(now);
        if ok {
            self.dropping = false;
            VideoVerdict::Pass
        } else {
            let gap = !self.dropping;
            self.dropping = true;
            VideoVerdict::Drop { gap }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_refills_and_caps() {
        let t0 = Instant::now();
        let mut b = Bucket::every(Duration::from_secs(1), 3, t0);
        assert!(b.take(t0) && b.take(t0) && b.take(t0));
        assert!(!b.take(t0));
        assert!(b.take(t0 + Duration::from_millis(1000)));
        assert!(!b.take(t0 + Duration::from_millis(1100)));
        // Long idle: refilled to the burst, not beyond.
        let later = t0 + Duration::from_secs(100);
        assert!(b.take(later) && b.take(later) && b.take(later));
        assert!(!b.take(later));
    }

    #[test]
    fn keyed_buckets_are_independent_and_bounded() {
        let t0 = Instant::now();
        let mut k: Buckets<u32> = Buckets::new(Duration::from_secs(2), 1);
        assert!(k.take(&1, t0));
        assert!(!k.take(&1, t0));
        assert!(k.take(&2, t0));
        for i in 0..(MAX_KEYS as u32 * 2) {
            k.take(&(100 + i), t0);
        }
        assert!(k.map.len() <= MAX_KEYS);
        // Once idle again, old keys are forgotten to make room.
        assert!(k.take(&999_999, t0 + Duration::from_secs(10)));
    }

    #[test]
    fn audio_policer_passes_honest_voice_and_cuts_a_flood() {
        let t0 = Instant::now();
        let mut p = AudioPolicer::new(t0);
        // 10 s of honest voice: 50 packets/s of 150 bytes, all forwarded.
        for i in 0..500u64 {
            assert!(p.allow(150, t0 + Duration::from_millis(i * 20)), "honest packet {i}");
        }
        assert_eq!(p.dropped, 0);
        // 10 s of voice plus a soundboard stream (100 packets/s), arriving in 40 ms clumps.
        let mut s = AudioPolicer::new(t0);
        for i in 0..250u64 {
            let at = t0 + Duration::from_millis(i * 40);
            for k in 0..4 {
                assert!(s.allow(200, at), "two streams, clump {i} packet {k}");
            }
        }
        assert_eq!(s.dropped, 0);
        // A flood of 1200-byte datagrams at 5000/s for one second: bounded.
        let mut q = AudioPolicer::new(t0);
        let passed = (0..5000u64).filter(|i| q.allow(1200, t0 + Duration::from_micros(i * 200))).count();
        assert!(passed <= 150, "{passed} flood packets passed");
        // Oversized datagrams never pass.
        let mut r = AudioPolicer::new(t0);
        assert!(!r.allow(tincan_core::media::AUDIO_HEADER_LEN + OPUS_MAX_PACKET + 1, t0));
    }

    #[test]
    fn video_policer_passes_honest_shares_and_cuts_floods() {
        let t0 = Instant::now();
        // 30 KB frames at 30 fps (about 7 Mbit/s, above any honest target) pass.
        let mut p = VideoPolicer::new(t0);
        for i in 0..300u64 {
            assert_eq!(p.check(30_000, i % 90 == 0, t0 + Duration::from_millis(i * 33)), VideoVerdict::Pass, "frame {i}");
        }
        // 50 Mbit/s for 2 s is cut to the cap (plus the burst), and drops run to a keyframe.
        let mut q = VideoPolicer::new(t0);
        let mut passed = 0usize;
        let mut gaps = 0;
        for i in 0..120u64 {
            match q.check(200_000, i % 30 == 0, t0 + Duration::from_micros(i * 16_666)) {
                VideoVerdict::Pass => passed += 200_000,
                VideoVerdict::Drop { gap } => gaps += usize::from(gap),
            }
        }
        let cap = (f64::from(VIDEO_INGRESS_BPS) / 8.0 * 2.0 + VIDEO_INGRESS_BURST) as usize;
        assert!(passed <= cap, "{passed} bytes passed, cap {cap}");
        assert!(gaps >= 1);
        // Frame rate: 1000 tiny frames in one second are cut to about 60.
        let mut f = VideoPolicer::new(t0);
        let n = (0..1000u64).filter(|i| f.check(10, true, t0 + Duration::from_millis(*i)) == VideoVerdict::Pass).count();
        assert!(n <= 2 * VIDEO_MAX_FPS as usize, "{n} frames passed");
    }
}
