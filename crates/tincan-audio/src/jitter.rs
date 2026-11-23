//! Adaptive per-track jitter buffer.
//!
//! The buffer decides, every 20 ms, what one remote track plays next ([`Playout`]); decoding is
//! done by the caller. Time is passed in explicitly (milliseconds on any monotonic clock), so
//! the logic is deterministic and unit-testable.
//!
//! Behaviour (see `docs/architecture.md`, "Audio"):
//! - Delay bounds min 40 / start 60 / max 200 ms (300 ms on relayed paths).
//! - Target = max(min, P95 of the relative arrival delay over the last 2 s + 1 frame).
//! - A packet that arrives after its slot was already concealed with an empty buffer is an
//!   underrun: the concealment frames have already added delay and the target grows by one
//!   frame for 2 s.
//! - When the buffered audio stays above target + 20 ms for 1 s, one frame is dropped, preferably
//!   a quiet one or a frame of a DTX pause (after 3 s any frame, so a continuous signal cannot pin
//!   the latency high).
//! - A missing frame with later packets buffered is lost: decode in-band FEC from the next packet
//!   if it is buffered, otherwise Opus PLC, for up to 5 frames, then a faded frame, then silence.
//! - DTX and mute pauses (sequence numbers contiguous, timestamp jumps) play as silence; a pause
//!   that drains the buffer re-anchors the playout at the next talkspurt.
//! - Sequence numbers wrap; reordering within the buffer depth is undone; duplicates and late
//!   packets are dropped; a far jump means the sender restarted and the buffer resets.

use bytes::Bytes;
use tincan_core::media::{AF_SPEAKING, AudioHeader};

/// Samples per Opus frame (20 ms at 48 kHz).
pub const FRAME_SAMPLES: usize = 960;
/// Frame duration in milliseconds.
pub const FRAME_MS: u32 = 20;
/// Consecutive concealment frames before the output fades to silence.
pub const MAX_CONCEAL_FRAMES: u8 = 5;

const FRAME_TS: u32 = FRAME_SAMPLES as u32;
const TS_PER_MS: i64 = 48;
const SLOTS: usize = 64;
const LATE_WINDOW: i32 = 64;
const REORDER_WINDOW: i32 = 16;
const WINDOW_MS: u64 = 2000;
const SHRINK_HOLD_MS: u64 = 1000;
const FORCE_SHRINK_MS: u64 = 3000;
const BOOST_MS: u64 = 2000;
const QUIET_LEVEL: u8 = 50;
const HIST: usize = 128;
const MIN_SAMPLES_FOR_TARGET: usize = 5;

/// Delay bounds of a jitter buffer in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitterConfig {
    /// Lowest target delay.
    pub min_ms: u32,
    /// Initial target delay before any statistics exist.
    pub start_ms: u32,
    /// Highest target delay; audio buffered beyond max + one frame is dropped.
    pub max_ms: u32,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self { min_ms: 40, start_ms: 60, max_ms: 200 }
    }
}

impl JitterConfig {
    /// Bounds for a relayed path (max raised to 300 ms).
    pub fn relayed() -> Self {
        Self { max_ms: 300, ..Self::default() }
    }

    fn sanitized(self) -> Self {
        let max_ms = self.max_ms.clamp(FRAME_MS * 2, (SLOTS as u32 - 4) * FRAME_MS);
        let min_ms = self.min_ms.clamp(FRAME_MS, max_ms);
        let start_ms = self.start_ms.clamp(min_ms, max_ms);
        Self { min_ms, start_ms, max_ms }
    }
}

/// What the track should play for the next 20 ms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Playout {
    /// Nothing to play (not started, DTX pause, after fade-out). Output zeros.
    Silence,
    /// Decode this Opus payload normally.
    Decode(Bytes),
    /// The frame is lost; reconstruct it from the in-band FEC of this (next) payload.
    Fec(Bytes),
    /// The frame is missing; run Opus packet loss concealment.
    Conceal,
    /// Last concealment frame: run PLC and fade it out to silence.
    ConcealFadeOut,
}

/// Counters for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitterStats {
    /// Packets offered to `push`.
    pub received: u64,
    /// Packets dropped because the same sequence number was already buffered.
    pub duplicates: u64,
    /// Packets dropped because their playout time had passed.
    pub late: u64,
    /// Frames skipped as lost (later packets were buffered).
    pub lost: u64,
    /// Frames reconstructed from in-band FEC.
    pub fec: u64,
    /// Opus PLC frames (loss and underrun).
    pub concealed: u64,
    /// Late arrivals of frames that had been concealed on an empty buffer.
    pub underruns: u64,
    /// Frames dropped to shrink the delay.
    pub shrink_drops: u64,
    /// Frames dropped because more than max delay was buffered.
    pub overflow_drops: u64,
    /// Stream restarts (far sequence jump).
    pub resets: u64,
    /// Playout re-anchors at a talkspurt start.
    pub talkspurts: u64,
}

#[derive(Clone, Debug)]
struct Slot {
    seq: u16,
    ts: u32,
    flags: u8,
    level: u8,
    arrival_ms: u64,
    payload: Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Buffering { start_at_ms: u64 },
    Playing,
}

/// Adaptive jitter buffer for one remote audio track. See the module docs.
#[derive(Debug)]
pub struct JitterBuffer {
    cfg: JitterConfig,
    slots: Vec<Option<Slot>>,
    phase: Phase,
    synced: bool,
    next_seq: u16,
    next_ts: u32,
    starving: bool,
    starve_frames: u32,
    conceal_run: u8,
    target_ms: u32,
    boost_ms: u32,
    boost_until_ms: u64,
    above_since_ms: Option<u64>,
    hist: [(u64, f64); HIST],
    hist_head: usize,
    hist_len: usize,
    ext_ts: i64,
    last_ts: u32,
    stats: JitterStats,
}

/// Signed distance `a - b` on the 16-bit sequence circle.
fn seq_diff(a: u16, b: u16) -> i32 {
    i32::from(a.wrapping_sub(b) as i16)
}

impl JitterBuffer {
    /// Creates an empty buffer.
    pub fn new(cfg: JitterConfig) -> Self {
        let cfg = cfg.sanitized();
        Self {
            cfg,
            slots: vec![None; SLOTS],
            phase: Phase::Idle,
            synced: false,
            next_seq: 0,
            next_ts: 0,
            starving: false,
            starve_frames: 0,
            conceal_run: 0,
            target_ms: cfg.start_ms,
            boost_ms: 0,
            boost_until_ms: 0,
            above_since_ms: None,
            hist: [(0, 0.0); HIST],
            hist_head: 0,
            hist_len: 0,
            ext_ts: 0,
            last_ts: 0,
            stats: JitterStats::default(),
        }
    }

    /// Changes the delay bounds (for example when the path becomes relayed).
    pub fn set_config(&mut self, cfg: JitterConfig) {
        self.cfg = cfg.sanitized();
        self.target_ms = self.target_ms.clamp(self.cfg.min_ms, self.cfg.max_ms);
    }

    /// Current target delay in milliseconds.
    pub fn target_ms(&self) -> u32 {
        self.target_ms
    }

    /// Counters.
    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    /// True when nothing is playing or buffering (after a fade-out or before the first packet).
    pub fn is_idle(&self) -> bool {
        self.phase == Phase::Idle
    }

    /// Audio buffered ahead of the playout point, in milliseconds.
    pub fn level_ms(&self) -> u32 {
        let Some((k, slot)) = self.highest() else { return 0 };
        let by_ts = (i64::from(slot.ts.wrapping_sub(self.next_ts) as i32) + i64::from(FRAME_TS)) / TS_PER_MS;
        let by_seq = (k as i64 + 1) * i64::from(FRAME_MS);
        if (0..=i64::from(SLOTS as u32 * FRAME_MS * 4)).contains(&by_ts) { by_ts as u32 } else { by_seq as u32 }
    }

    /// Offers one received packet. `payload` is the Opus packet (after the 12-byte header).
    pub fn push(&mut self, h: &AudioHeader, payload: Bytes, now_ms: u64) {
        self.stats.received += 1;
        if !self.synced {
            self.begin_stream(h, now_ms);
        } else {
            let d = seq_diff(h.seq, self.next_seq);
            match self.phase {
                Phase::Idle => {
                    if (0..SLOTS as i32).contains(&d) {
                        // first packet after a pause or a fade-out: re-anchor
                        self.clear_slots();
                        self.next_seq = h.seq;
                        self.next_ts = h.ts;
                        self.phase = Phase::Buffering { start_at_ms: now_ms + u64::from(self.target_ms) };
                        self.stats.talkspurts += 1;
                    } else if (-LATE_WINDOW..0).contains(&d) {
                        self.stats.late += 1;
                        return;
                    } else {
                        self.reset_stream(h, now_ms);
                    }
                }
                Phase::Buffering { .. } => {
                    if d < 0 {
                        if d >= -REORDER_WINDOW {
                            // an earlier frame of the spurt arrived after a later one
                            self.next_seq = h.seq;
                            self.next_ts = h.ts;
                        } else if d >= -LATE_WINDOW {
                            self.stats.late += 1;
                            return;
                        } else {
                            self.reset_stream(h, now_ms);
                        }
                    } else if d >= SLOTS as i32 {
                        self.reset_stream(h, now_ms);
                    }
                }
                Phase::Playing => {
                    if d < 0 {
                        if d >= -LATE_WINDOW {
                            self.stats.late += 1;
                            return;
                        }
                        self.reset_stream(h, now_ms);
                    } else if d >= SLOTS as i32 {
                        self.reset_stream(h, now_ms);
                    } else if d == 0 && self.starving && h.ts == self.next_ts {
                        // the frame we have been concealing on an empty buffer arrived late
                        self.stats.underruns += 1;
                        self.boost_ms = (self.target_ms + FRAME_MS).min(self.cfg.max_ms);
                        self.boost_until_ms = now_ms + BOOST_MS;
                    }
                }
            }
        }
        let idx = usize::from(h.seq) % SLOTS;
        if self.slots[idx].as_ref().is_some_and(|s| s.seq == h.seq) {
            self.stats.duplicates += 1;
            return;
        }
        self.slots[idx] = Some(Slot { seq: h.seq, ts: h.ts, flags: h.flags, level: h.level, arrival_ms: now_ms, payload });
        self.record_delay(h.ts, now_ms);
    }

    /// Decides what to play for the next 20 ms. Call once per frame period.
    pub fn pull(&mut self, now_ms: u64) -> Playout {
        self.update_target(now_ms);
        match self.phase {
            Phase::Idle => return Playout::Silence,
            Phase::Buffering { start_at_ms } => {
                if now_ms < start_at_ms && self.level_ms() < self.cfg.max_ms {
                    return Playout::Silence;
                }
                self.phase = Phase::Playing;
                self.starving = false;
                self.starve_frames = 0;
                self.conceal_run = 0;
                self.above_since_ms = None;
            }
            Phase::Playing => {}
        }
        self.trim_overflow();

        if self.starving && self.peek(self.next_seq).is_none() && self.has_later() {
            // Later frames arrived while we were concealing on an empty buffer: the missing
            // frames were lost, and the concealment already covered `starve_frames` of them.
            let mut covered = self.starve_frames;
            while covered > 0 && self.peek(self.next_seq).is_none() {
                self.stats.lost += 1;
                self.next_seq = self.next_seq.wrapping_add(1);
                self.next_ts = self.next_ts.wrapping_add(FRAME_TS);
                covered -= 1;
            }
            self.starving = false;
            self.starve_frames = 0;
        }

        let mut dropped = false;
        loop {
            let level = self.level_ms();
            if level > self.target_ms + FRAME_MS {
                self.above_since_ms.get_or_insert(now_ms);
            } else {
                self.above_since_ms = None;
            }
            let excess_for = self.above_since_ms.map(|s| now_ms.saturating_sub(s)).filter(|&e| e >= SHRINK_HOLD_MS);

            let Some(slot) = self.take(self.next_seq) else { break };
            let jump = slot.ts.wrapping_sub(self.next_ts) as i32;
            if jump >= FRAME_TS as i32 {
                // a DTX or mute pause precedes this packet
                if self.starving || jump as u32 > self.cfg.max_ms * TS_PER_MS as u32 {
                    // the pause drained the buffer: re-anchor at this talkspurt
                    let start = slot.arrival_ms.max(now_ms.saturating_sub(u64::from(self.target_ms)))
                        + u64::from(self.target_ms);
                    self.next_ts = slot.ts;
                    self.put_back(slot);
                    self.phase = Phase::Buffering { start_at_ms: start };
                    self.stats.talkspurts += 1;
                    let out = self.end_concealment();
                    self.starving = false;
                    self.starve_frames = 0;
                    self.conceal_run = 0;
                    return out;
                }
                // the pause is shorter than the buffer: play it as silence (shrink inside it)
                self.put_back(slot);
                let mut advance = FRAME_TS;
                if excess_for.is_some() && jump as u32 >= 2 * FRAME_TS {
                    advance = 2 * FRAME_TS;
                    self.stats.shrink_drops += 1;
                    self.above_since_ms = None;
                }
                self.next_ts = self.next_ts.wrapping_add(advance);
                let out = self.end_concealment();
                self.conceal_run = 0;
                return out;
            }
            if let Some(e) = excess_for {
                let quiet = slot.flags & AF_SPEAKING == 0 || slot.level >= QUIET_LEVEL;
                if !dropped && (quiet || e >= FORCE_SHRINK_MS) && level > 2 * FRAME_MS {
                    dropped = true;
                    self.stats.shrink_drops += 1;
                    self.above_since_ms = None;
                    self.next_seq = self.next_seq.wrapping_add(1);
                    self.next_ts = slot.ts.wrapping_add(FRAME_TS);
                    continue;
                }
            }
            self.next_seq = self.next_seq.wrapping_add(1);
            self.next_ts = slot.ts.wrapping_add(FRAME_TS);
            self.starving = false;
            self.starve_frames = 0;
            self.conceal_run = 0;
            return Playout::Decode(slot.payload);
        }

        if self.has_later() {
            // lost frame: conceal it and move on
            self.stats.lost += 1;
            self.starving = false;
            self.starve_frames = 0;
            let fec = self.peek(self.next_seq.wrapping_add(1)).map(|next| next.payload.clone());
            let out = if let Some(next) = fec {
                self.stats.fec += 1;
                self.conceal_run = 0;
                Playout::Fec(next)
            } else if self.conceal_run < MAX_CONCEAL_FRAMES {
                self.stats.concealed += 1;
                self.conceal_run += 1;
                Playout::Conceal
            } else if self.conceal_run == MAX_CONCEAL_FRAMES {
                self.conceal_run += 1;
                Playout::ConcealFadeOut
            } else {
                Playout::Silence
            };
            self.next_seq = self.next_seq.wrapping_add(1);
            self.next_ts = self.next_ts.wrapping_add(FRAME_TS);
            return out;
        }

        // Empty buffer: an underrun or the end of a talkspurt. Conceal without advancing, so a
        // late packet still plays (the concealment frames become extra delay).
        self.starving = true;
        self.starve_frames = self.starve_frames.saturating_add(1);
        if self.conceal_run < MAX_CONCEAL_FRAMES {
            self.conceal_run += 1;
            self.stats.concealed += 1;
            Playout::Conceal
        } else if self.conceal_run == MAX_CONCEAL_FRAMES {
            self.conceal_run += 1;
            Playout::ConcealFadeOut
        } else {
            self.phase = Phase::Idle;
            Playout::Silence
        }
    }

    fn end_concealment(&self) -> Playout {
        if (1..=MAX_CONCEAL_FRAMES).contains(&self.conceal_run) { Playout::ConcealFadeOut } else { Playout::Silence }
    }

    fn begin_stream(&mut self, h: &AudioHeader, now_ms: u64) {
        self.synced = true;
        self.next_seq = h.seq;
        self.next_ts = h.ts;
        self.phase = Phase::Buffering { start_at_ms: now_ms + u64::from(self.target_ms) };
        self.starving = false;
        self.starve_frames = 0;
        self.conceal_run = 0;
        self.above_since_ms = None;
        self.hist_len = 0;
        self.ext_ts = 0;
        self.last_ts = h.ts;
    }

    fn reset_stream(&mut self, h: &AudioHeader, now_ms: u64) {
        self.stats.resets += 1;
        self.clear_slots();
        self.begin_stream(h, now_ms);
    }

    fn clear_slots(&mut self) {
        for s in &mut self.slots {
            *s = None;
        }
    }

    fn peek(&self, seq: u16) -> Option<&Slot> {
        self.slots[usize::from(seq) % SLOTS].as_ref().filter(|s| s.seq == seq)
    }

    fn take(&mut self, seq: u16) -> Option<Slot> {
        let idx = usize::from(seq) % SLOTS;
        if self.slots[idx].as_ref().is_some_and(|s| s.seq == seq) { self.slots[idx].take() } else { None }
    }

    fn put_back(&mut self, slot: Slot) {
        let idx = usize::from(slot.seq) % SLOTS;
        self.slots[idx] = Some(slot);
    }

    /// Highest buffered slot at or after the playout point: (offset in frames, slot).
    fn highest(&self) -> Option<(usize, &Slot)> {
        (0..SLOTS).rev().find_map(|k| self.peek(self.next_seq.wrapping_add(k as u16)).map(|s| (k, s)))
    }

    fn has_later(&self) -> bool {
        (1..SLOTS).any(|k| self.peek(self.next_seq.wrapping_add(k as u16)).is_some())
    }

    fn trim_overflow(&mut self) {
        let limit = self.cfg.max_ms + FRAME_MS;
        for _ in 0..SLOTS {
            if self.level_ms() <= limit {
                break;
            }
            self.stats.overflow_drops += 1;
            match self.take(self.next_seq) {
                Some(s) => self.next_ts = s.ts.wrapping_add(FRAME_TS),
                None => self.next_ts = self.next_ts.wrapping_add(FRAME_TS),
            }
            self.next_seq = self.next_seq.wrapping_add(1);
        }
    }

    fn record_delay(&mut self, ts: u32, now_ms: u64) {
        let ext = self.ext_ts + i64::from(ts.wrapping_sub(self.last_ts) as i32);
        self.ext_ts = ext;
        self.last_ts = ts;
        let transit = now_ms as f64 - ext as f64 / TS_PER_MS as f64;
        self.hist[self.hist_head] = (now_ms, transit);
        self.hist_head = (self.hist_head + 1) % HIST;
        self.hist_len = (self.hist_len + 1).min(HIST);
    }

    fn update_target(&mut self, now_ms: u64) {
        let mut delays = [0f64; HIST];
        let mut n = 0;
        let mut min = f64::MAX;
        for i in 0..self.hist_len {
            let (at, transit) = self.hist[(self.hist_head + HIST - 1 - i) % HIST];
            if now_ms.saturating_sub(at) > WINDOW_MS {
                break;
            }
            delays[n] = transit;
            min = min.min(transit);
            n += 1;
        }
        let mut target = self.target_ms;
        if n >= MIN_SAMPLES_FOR_TARGET {
            let d = &mut delays[..n];
            for v in d.iter_mut() {
                *v -= min;
            }
            d.sort_unstable_by(f64::total_cmp);
            let idx = ((n as f64 * 0.95).ceil() as usize).clamp(1, n) - 1;
            target = (d[idx] + f64::from(FRAME_MS)).round() as u32;
        }
        target = target.clamp(self.cfg.min_ms, self.cfg.max_ms);
        if now_ms < self.boost_until_ms {
            target = target.max(self.boost_ms);
        }
        self.target_ms = target;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tincan_core::media::AF_TALKSPURT;

    fn hdr(seq: u16, ts: u32, flags: u8) -> AudioHeader {
        AudioHeader { flags, track: 4, seq, ts, level: 20, epoch: 0 }
    }

    fn payload(seq: u16) -> Bytes {
        Bytes::copy_from_slice(&seq.to_be_bytes())
    }

    fn seq_of(p: &Bytes) -> u16 {
        u16::from_be_bytes([p[0], p[1]])
    }

    /// Event-driven simulation: `arrivals` (time, seq, ts, flags) sorted or not; pulls every
    /// 20 ms from `t0` for `pulls` frames. Returns the decisions.
    fn run(jb: &mut JitterBuffer, mut arrivals: Vec<(u64, u16, u32, u8)>, t0: u64, pulls: usize) -> Vec<Playout> {
        arrivals.sort_by_key(|a| a.0);
        let mut out = Vec::new();
        let mut i = 0;
        for p in 0..pulls {
            let now = t0 + p as u64 * 20;
            while i < arrivals.len() && arrivals[i].0 <= now {
                let (_, seq, ts, flags) = arrivals[i];
                jb.push(&hdr(seq, ts, flags), payload(seq), arrivals[i].0);
                i += 1;
            }
            out.push(jb.pull(now));
        }
        out
    }

    fn decoded(out: &[Playout]) -> Vec<u16> {
        out.iter().filter_map(|p| if let Playout::Decode(b) = p { Some(seq_of(b)) } else { None }).collect()
    }

    fn steady(n: u16, seq0: u16, ts0: u32, delay: impl Fn(u16) -> u64) -> Vec<(u64, u16, u32, u8)> {
        (0..n)
            .map(|k| (u64::from(k) * 20 + delay(k), seq0.wrapping_add(k), ts0.wrapping_add(u32::from(k) * 960), AF_SPEAKING))
            .collect()
    }

    #[test]
    fn steady_stream_plays_in_order_after_start_delay() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let out = run(&mut jb, steady(100, 1000, 5, |_| 0), 0, 110);
        let first = out.iter().position(|p| matches!(p, Playout::Decode(_))).unwrap();
        assert_eq!(first, 3, "start delay 60 ms = 3 frames");
        let seqs = decoded(&out);
        assert_eq!(seqs.len(), 100);
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1));
        assert_eq!(jb.stats().lost, 0);
        assert_eq!(jb.stats().underruns, 0);
    }

    #[test]
    fn seq_and_ts_wrap() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let out = run(&mut jb, steady(40, 65_520, u32::MAX - 5_000, |_| 0), 0, 50);
        let seqs = decoded(&out);
        assert_eq!(seqs.len(), 40);
        assert_eq!(seqs[0], 65_520);
        assert!(seqs.windows(2).all(|w| w[1] == w[0].wrapping_add(1)));
        assert_eq!(jb.stats().resets, 0);
    }

    #[test]
    fn duplicates_dropped_and_reordering_undone() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let mut arr = steady(50, 0, 0, |k| if k % 2 == 0 { 25 } else { 0 });
        arr.push((205, 10, 9600, AF_SPEAKING)); // duplicate of seq 10 (arrived at ~200)
        arr.push((215, 11, 10560, AF_SPEAKING)); // duplicate of seq 11
        let out = run(&mut jb, arr, 0, 70);
        let seqs = decoded(&out);
        assert_eq!(seqs, (0..50).collect::<Vec<u16>>());
        assert_eq!(jb.stats().duplicates, 2);
    }

    #[test]
    fn fec_used_for_single_loss_plc_for_bursts_then_fade() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        // lose 10, and 20..=27 (8 frames)
        let arr: Vec<_> = steady(60, 0, 0, |_| 0).into_iter().filter(|a| a.1 != 10 && !(20..=27).contains(&a.1)).collect();
        let out = run(&mut jb, arr, 0, 70);
        let at = |seq: usize| &out[seq + 3];
        assert_eq!(at(10), &Playout::Fec(payload(11)));
        assert_eq!(at(11), &Playout::Decode(payload(11)));
        for s in 20..25 {
            assert_eq!(at(s), &Playout::Conceal, "frame {s}");
        }
        assert_eq!(at(25), &Playout::ConcealFadeOut);
        assert_eq!(at(26), &Playout::Silence);
        assert_eq!(at(27), &Playout::Fec(payload(28)), "FEC from the next packet once available");
        assert_eq!(at(28), &Playout::Decode(payload(28)));
        let st = jb.stats();
        assert_eq!(st.lost, 9);
        assert_eq!(st.fec, 2);
        // 5 for the burst, 5 more after the last packet (end of stream)
        assert_eq!(st.concealed, 10);
    }

    #[test]
    fn underrun_conceals_then_plays_late_packet_and_grows() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        // packets 30.. arrive 90 ms late (more than the 60 ms buffer)
        let arr = steady(80, 0, 0, |k| if k >= 30 { 90 } else { 0 });
        let out = run(&mut jb, arr, 0, 100);
        let seqs = decoded(&out);
        assert_eq!(seqs, (0..80).collect::<Vec<u16>>(), "nothing skipped, the delay grew instead");
        assert!(out.contains(&Playout::Conceal));
        let st = jb.stats();
        assert_eq!(st.underruns, 1);
        assert_eq!(st.lost, 0);
        assert_eq!(st.late, 0);
    }

    #[test]
    fn target_follows_p95_and_grows_with_jitter() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        // pseudo-random 0..=100 ms jitter
        let arr = steady(200, 0, 0, |k| {
            let x = (u64::from(k) + 7).wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (x >> 33) % 101
        });
        let _ = run(&mut jb, arr, 0, 220);
        assert!(jb.target_ms() >= 100, "target {} should cover ~95 ms of jitter + 1 frame", jb.target_ms());
        assert!(jb.target_ms() <= 200);

        let mut calm = JitterBuffer::new(JitterConfig::default());
        let _ = run(&mut calm, steady(200, 0, 0, |_| 0), 0, 220);
        assert_eq!(calm.target_ms(), 40, "no jitter: target falls to the minimum");
    }

    #[test]
    fn late_packet_dropped() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        // seq 10 arrives 200 ms late, long after it was concealed as lost
        let arr = steady(40, 0, 0, |k| if k == 10 { 200 } else { 0 });
        let out = run(&mut jb, arr, 0, 50);
        assert!(!decoded(&out).contains(&10));
        assert_eq!(jb.stats().late, 1);
        assert_eq!(jb.stats().fec, 1);
    }

    #[test]
    fn shrinks_by_dropping_quiet_frames() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        // the first 10 packets arrive together at 180 ms (a stall), then a steady stream;
        // frames are quiet (no speaking flag) so shrinking may drop them.
        let arr: Vec<_> = (0u16..700)
            .map(|k| {
                let t = if k < 10 { 180 } else { u64::from(k) * 20 };
                (t, k, u32::from(k) * 960, 0u8)
            })
            .collect();
        let mut out = Vec::new();
        let mut i = 0;
        let mut levels = Vec::new();
        for p in 0..720u64 {
            let now = p * 20;
            while i < arr.len() && arr[i].0 <= now {
                let (_, seq, ts, flags) = arr[i];
                jb.push(&hdr(seq, ts, flags), payload(seq), arr[i].0);
                i += 1;
            }
            out.push(jb.pull(now));
            levels.push(jb.level_ms());
        }
        assert!(jb.stats().shrink_drops >= 1, "{:?}", jb.stats());
        // one frame per second of excess: from 220 ms down to the target takes several seconds
        let tail_max = levels[600..690].iter().copied().max().unwrap();
        assert!(tail_max <= jb.target_ms() + 2 * FRAME_MS, "level {tail_max} target {}", jb.target_ms());
    }

    #[test]
    fn dtx_pause_reanchors_without_growing() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let mut arr = steady(30, 0, 0, |_| 0);
        // 1 s of DTX: seq continues at 30, ts jumps by 1 s + 30 frames
        for k in 0..30u16 {
            let t = 1600 + u64::from(k) * 20;
            arr.push((t, 30 + k, (30 + 50 + u32::from(k)) * 960, AF_SPEAKING | if k == 0 { AF_TALKSPURT } else { 0 }));
        }
        let out = run(&mut jb, arr, 0, 120);
        let seqs = decoded(&out);
        assert_eq!(seqs, (0..60).collect::<Vec<u16>>());
        let st = jb.stats();
        assert_eq!(st.underruns, 0);
        assert_eq!(st.lost, 0);
        assert!(st.talkspurts >= 1);
        // second spurt starts one start delay after its first packet (1600 ms -> pull at >= 1660)
        let second = out.iter().position(|p| *p == Playout::Decode(payload(30))).unwrap();
        let t = second as u64 * 20;
        assert!((1640..=1700).contains(&t), "second spurt started at {t}");
    }

    #[test]
    fn short_pause_inside_buffer_plays_silence() {
        let mut jb = JitterBuffer::new(JitterConfig { min_ms: 80, start_ms: 80, max_ms: 200 });
        // frames 0..20, a 2-frame DTX gap (ts jumps 3 frames), frames continue on time
        let arr: Vec<_> = (0u16..40)
            .map(|k| {
                let slot = if k < 20 { u32::from(k) } else { u32::from(k) + 2 };
                (u64::from(slot) * 20, k, slot * 960, AF_SPEAKING)
            })
            .collect();
        let out = run(&mut jb, arr, 0, 50);
        let i19 = out.iter().position(|p| *p == Playout::Decode(payload(19))).unwrap();
        assert_eq!(out[i19 + 1], Playout::Silence);
        assert_eq!(out[i19 + 2], Playout::Silence);
        assert_eq!(out[i19 + 3], Playout::Decode(payload(20)));
        assert_eq!(jb.stats().underruns, 0);
    }

    #[test]
    fn sender_restart_resets() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let mut arr = steady(20, 100, 0, |_| 0);
        arr.extend(steady(20, 40_000, 777, |_| 0).into_iter().map(|(t, s, ts, f)| (t + 600, s, ts, f)));
        let out = run(&mut jb, arr, 0, 60);
        let seqs = decoded(&out);
        assert!(seqs.contains(&100) && seqs.contains(&40_019));
        assert_eq!(jb.stats().resets, 1);
    }

    #[test]
    fn overflow_is_trimmed_to_max() {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        // 30 frames (600 ms) arrive at once
        let arr: Vec<_> = (0u16..30).map(|k| (0u64, k, u32::from(k) * 960, AF_SPEAKING)).collect();
        let _ = run(&mut jb, arr, 0, 2);
        assert!(jb.level_ms() <= 220, "level {}", jb.level_ms());
        assert!(jb.stats().overflow_drops >= 9);
    }
}
