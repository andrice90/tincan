//! Receive statistics (for `ReceiverReport`s) and the AIMD bitrate controller.

use std::collections::HashMap;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::time::Instant;

use super::{VIDEO_MAX_BPS, VIDEO_MIN_BPS};

/// A seq jump this large (either way) means the sender restarted.
const SEQ_RESTART: i32 = 1000;
/// Clean time before the target grows again.
const AIMD_INCREASE_EVERY: Duration = Duration::from_secs(4);
/// A new target is sent only if it moved at least this many percent.
const AIMD_SEND_DELTA_PCT: u64 = 5;

/// One report window of a track.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Report {
    /// Packets (audio) or frames (video) received.
    pub rx: u32,
    /// Lost (seq gaps; video: skipped or reset groups).
    pub lost: u32,
    /// Arrived out of order, or too late to hand on.
    pub late: u32,
    /// Interarrival jitter (RFC 3550 style), ms.
    pub jitter_ms: u16,
}

impl Report {
    /// Loss in percent of what was sent.
    pub fn loss_pct(&self) -> u8 {
        let total = u64::from(self.rx) + u64::from(self.lost);
        if total == 0 {
            return 0;
        }
        u8::try_from(u64::from(self.lost) * 100 / total).unwrap_or(100)
    }
}

/// Receive counters of one track.
#[derive(Debug, Default)]
pub(crate) struct TrackRx {
    win: Report,
    next_seq: Option<u16>,
    jitter: f32,
    last: Option<(Instant, u32)>,
}

impl TrackRx {
    /// An audio datagram with `seq` and 48 kHz `ts` arrived at `now`.
    pub fn audio(&mut self, seq: u16, ts: u32, now: Instant) {
        self.win.rx = self.win.rx.saturating_add(1);
        match self.next_seq {
            None => self.next_seq = Some(seq.wrapping_add(1)),
            Some(expected) => {
                let d = i32::from(seq.wrapping_sub(expected) as i16);
                if d == 0 || d.abs() >= SEQ_RESTART {
                    self.next_seq = Some(seq.wrapping_add(1));
                } else if d > 0 {
                    self.win.lost = self.win.lost.saturating_add(d.unsigned_abs());
                    self.next_seq = Some(seq.wrapping_add(1));
                } else {
                    // Reordered: it was counted as lost when the gap opened.
                    self.win.late = self.win.late.saturating_add(1);
                    self.win.lost = self.win.lost.saturating_sub(1);
                }
            }
        }
        self.jitter(ts, 48, now);
    }

    /// A video frame with 90 kHz `ts` arrived at `now`.
    pub fn video(&mut self, ts: u32, now: Instant) {
        self.win.rx = self.win.rx.saturating_add(1);
        self.jitter(ts, 90, now);
    }

    /// Video frames were lost (a reset or skipped group counts once).
    pub fn lost(&mut self, n: u32) { self.win.lost = self.win.lost.saturating_add(n); }

    /// A frame arrived but could not be handed on in time.
    pub fn late(&mut self) { self.win.late = self.win.late.saturating_add(1); }

    fn jitter(&mut self, ts: u32, khz: u32, now: Instant) {
        if let Some((t0, ts0)) = self.last {
            let arrival = now.saturating_duration_since(t0).as_secs_f32() * 1000.0;
            let media = ts.wrapping_sub(ts0) as i32 as f32 / khz as f32;
            let d = (arrival - media).abs();
            // A pause (DTX, an idle screen) is not jitter.
            if d < 1000.0 {
                self.jitter += (d - self.jitter) / 16.0;
            }
        }
        self.last = Some((now, ts));
    }

    /// The window's counters (and reset them), or None if nothing happened.
    pub fn take(&mut self) -> Option<Report> {
        let mut r = std::mem::take(&mut self.win);
        if r == Report::default() {
            return None;
        }
        r.jitter_ms = self.jitter.round().clamp(0.0, f32::from(u16::MAX)) as u16;
        Some(r)
    }
}

/// Counters of every track a session receives, shared with its reader tasks.
#[derive(Debug, Default)]
pub(crate) struct RxStats(Mutex<HashMap<u16, TrackRx>>);

impl RxStats {
    /// Update one track.
    pub fn with(&self, track: u16, f: impl FnOnce(&mut TrackRx)) { f(self.0.lock().entry(track).or_default()) }

    /// Reports of every track that saw activity since the last call.
    pub fn drain(&self) -> Vec<(u16, Report)> { self.0.lock().iter_mut().filter_map(|(t, s)| s.take().map(|r| (*t, r))).collect() }

    /// Forget a track.
    pub fn forget(&self, track: u16) { self.0.lock().remove(&track); }
}

/// AIMD target bitrate of one video track: x0.7 after a window with drops or loss, +10%
/// after every 4 s without, always within `[VIDEO_MIN_BPS, cap]`. The decrease starts from
/// the measured input rate when that is lower than the target, so it bites even when the
/// encoder already runs below the target.
#[derive(Debug)]
pub(crate) struct Aimd {
    target: u32,
    last_change: Instant,
    congested: bool,
    last_sent: Option<u32>,
    peak_bps: u32,
}

impl Aimd {
    /// Start at the maximum (the first tick clamps to the cap).
    pub fn new(now: Instant) -> Self { Self { target: VIDEO_MAX_BPS, last_change: now, congested: false, last_sent: None, peak_bps: 0 } }

    /// A viewer dropped frames or reported loss in this window.
    pub fn congestion(&mut self) { self.congested = true; }

    /// The track's input rate over the last window (peak-hold, decaying 10% per window).
    pub fn observe(&mut self, bps: u32) { self.peak_bps = bps.max(self.peak_bps / 10 * 9); }

    /// Current target.
    pub fn current(&self) -> u32 { self.target }

    /// Close the window. Returns the target to announce when it moved enough (or was never
    /// announced).
    pub fn tick(&mut self, now: Instant, cap: u32) -> Option<u32> {
        let cap = cap.clamp(VIDEO_MIN_BPS, VIDEO_MAX_BPS);
        if self.congested {
            self.congested = false;
            let base = if self.peak_bps > 0 { self.target.min(self.peak_bps) } else { self.target };
            self.target = base / 10 * 7;
            self.last_change = now;
        } else if now.saturating_duration_since(self.last_change) >= AIMD_INCREASE_EVERY {
            self.target = self.target.saturating_add(self.target / 10);
            self.last_change = now;
        }
        self.target = self.target.clamp(VIDEO_MIN_BPS, cap);
        let moved = match self.last_sent {
            None => true,
            Some(s) => u64::from(s.abs_diff(self.target)) * 100 >= u64::from(s) * AIMD_SEND_DELTA_PCT,
        };
        if moved {
            self.last_sent = Some(self.target);
            Some(self.target)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_seq_accounting() {
        let t0 = Instant::now();
        let mut s = TrackRx::default();
        for seq in [10u16, 11, 12, 14, 15] {
            s.audio(seq, u32::from(seq) * 960, t0 + Duration::from_millis(u64::from(seq) * 20));
        }
        let r = s.take().unwrap();
        assert_eq!((r.rx, r.lost, r.late), (5, 1, 0));
        assert_eq!(r.loss_pct(), 16);
        // The missing one shows up late: it is no longer lost.
        s.audio(16, 0, t0);
        s.audio(18, 0, t0);
        s.audio(17, 0, t0);
        let r = s.take().unwrap();
        assert_eq!((r.rx, r.lost, r.late), (3, 0, 1));
        // Wrap-around is contiguous.
        let mut w = TrackRx::default();
        for seq in [65534u16, 65535, 0, 1] {
            w.audio(seq, 0, t0);
        }
        assert_eq!(w.take().unwrap().lost, 0);
        // A restart is not counted as loss.
        w.audio(30_000, 0, t0);
        assert_eq!(w.take().unwrap().lost, 0);
        // Nothing happened: no report.
        assert!(w.take().is_none());
    }

    #[test]
    fn jitter_follows_arrival_spread() {
        let t0 = Instant::now();
        let mut steady = TrackRx::default();
        let mut jumpy = TrackRx::default();
        for i in 0..100u32 {
            steady.audio(i as u16, i * 960, t0 + Duration::from_millis(u64::from(i) * 20));
            let wobble = if i % 2 == 0 { 0 } else { 15 };
            jumpy.audio(i as u16, i * 960, t0 + Duration::from_millis(u64::from(i) * 20 + wobble));
        }
        assert!(steady.take().unwrap().jitter_ms <= 1);
        assert!(jumpy.take().unwrap().jitter_ms >= 8);
    }

    #[test]
    fn rx_stats_drain_only_active_tracks() {
        let s = RxStats::default();
        let now = Instant::now();
        s.with(4, |t| t.video(0, now));
        s.with(8, |t| t.lost(2));
        let mut d = s.drain();
        d.sort_by_key(|(t, _)| *t);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].1.rx, 1);
        assert_eq!(d[1].1.lost, 2);
        assert!(s.drain().is_empty());
        s.forget(4);
        s.with(4, |t| t.late());
        assert_eq!(s.drain()[0].1.late, 1);
    }

    #[test]
    fn aimd_decreases_on_congestion_and_recovers_slowly() {
        let t0 = Instant::now();
        let s = |n: u64| t0 + Duration::from_secs(n);
        let mut a = Aimd::new(t0);
        // First tick announces the capped start.
        assert_eq!(a.tick(s(1), 2_000_000), Some(2_000_000));
        // Steady and clean: nothing new to say until 4 s passed (and then the cap holds it).
        assert_eq!(a.tick(s(2), 2_000_000), None);
        assert_eq!(a.tick(s(5), 2_000_000), None);
        // A drop: x0.7 at once.
        a.congestion();
        assert_eq!(a.tick(s(6), 2_000_000), Some(1_400_000));
        // Clean again: +10% only after 4 s.
        assert_eq!(a.tick(s(7), 2_000_000), None);
        assert_eq!(a.tick(s(10), 2_000_000), Some(1_540_000));
        // The decrease starts from the measured rate when the encoder runs lower.
        a.observe(500_000);
        a.congestion();
        assert_eq!(a.tick(s(11), 2_000_000), Some(350_000));
        // Never below the floor.
        for i in 0..10 {
            a.congestion();
            a.tick(s(12 + i), 2_000_000);
        }
        assert_eq!(a.current(), VIDEO_MIN_BPS);
        // A lower cap (relayed viewer) applies at once.
        let mut b = Aimd::new(t0);
        assert_eq!(b.tick(s(1), 800_000), Some(800_000));
        assert_eq!(b.tick(s(2), 10_000_000), None, "rising needs 4 clean seconds");
        assert_eq!(b.tick(s(5), 10_000_000), Some(880_000));
    }

    #[test]
    fn peak_hold_decays() {
        let mut a = Aimd::new(Instant::now());
        a.observe(1_000_000);
        a.observe(0);
        assert_eq!(a.peak_bps, 900_000);
        a.observe(2_000_000);
        assert_eq!(a.peak_bps, 2_000_000);
    }
}
