//! One video track's forwarder: groups (one per keyframe), a GOP cache, viewers with bounded
//! queues and the drop policy, coalesced keyframe requests and the AIMD target.
//!
//! Used for every video hop: the sharer's upload to its host (one viewer: the host), the host
//! SFU (viewers: subscribed members and the host's own app) and a call (viewer: the peer).
//!
//! Remote viewers get one QUIC uni stream per group, written by a writer task fed from a
//! bounded queue. Drop policy (docs/architecture.md "Video transport"): when a viewer's queue
//! holds more than 2 frames and more than 250 ms of the track's rate, when it is full, or when
//! a frame waited more than 300 ms, that viewer's group stream is reset, it skips to the next
//! keyframe and a keyframe is requested upstream (at most once per second per track).
//!
//! Stream priority: the newest group's stream gets 50, the one it replaces is demoted to 0
//! before it is finished, so the control stream (100) always goes first and a new group
//! always beats the tail of an old one (a raw `set_priority(group_seq)` would pass 100 after
//! 100 groups).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use iroh::endpoint::{Connection, SendStream, VarInt, WriteError};
use parking_lot::Mutex;
use tincan_core::UserId;
use tincan_core::media::{CODEC_H264_ANNEXB, VideoFrame, VideoStreamHeader};
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use super::stats::Aimd;
use super::wire::{OutError, VideoOut, Wire};
use super::VIDEO_MIN_BPS;
use crate::link::Tasks;

/// Priority of the newest group's stream (the control stream has 100).
pub(crate) const PRIO_NEW: i32 = 50;
/// Priority a replaced group's stream is demoted to.
pub(crate) const PRIO_OLD: i32 = 0;
/// Stream reset code: the group was abandoned (skip to the next keyframe).
pub(crate) const VIDEO_RESET: u32 = 1;
/// A queued frame older than this is stale.
const STALE: Duration = Duration::from_millis(300);
/// A queue longer than this many frames may be overloaded...
const QUEUE_MIN_FRAMES: usize = 2;
/// ...when it also holds more than this share of a second of the track's rate (250 ms).
const QUEUE_MAX_SECONDS_DIV: u32 = 4;
/// Viewer queue capacity in frames (a 4 s cached group at 30 fps fits).
const VIEWER_QUEUE: usize = 160;
/// GOP cache bounds.
const CACHE_MAX_BYTES: usize = 2 * 1024 * 1024;
const CACHE_MAX_TS: u32 = 4 * 90_000;
/// A cached group older than this also triggers a keyframe request for a new viewer.
const CACHE_FRESH: Duration = Duration::from_millis(1500);
/// Input rate measurement window.
const RATE_WINDOW_S: f64 = 1.0;
/// Keyframe requests upstream are coalesced to one per this interval (also the most a
/// viewer is re-seeded from the cache).
pub(crate) const KEYFRAME_EVERY: Duration = Duration::from_secs(1);
/// A group stream that cannot be opened within this is skipped.
const OPEN_TIMEOUT: Duration = Duration::from_secs(2);

/// Who watches a track.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ViewerId {
    /// This device's app (`video_in`).
    Local,
    /// A remote peer (member, host or call peer).
    Peer(UserId),
}

/// Upstream keyframe request (called with the track id, outside the track's lock).
pub(crate) type KeyframeFn = Box<dyn Fn(u16) + Send + Sync>;

/// One video track.
pub(crate) struct VideoTrack {
    /// Track id (`track_id(short_id, KIND_SCREEN)`).
    pub track: u16,
    inner: Mutex<Inner>,
    keyframe: KeyframeFn,
    tasks: Arc<Tasks>,
    /// Remote viewers seeded from the cache so far (tests).
    #[cfg(test)]
    pub seeds: AtomicUsize,
    /// Keyframe requests sent to the source so far (tests).
    #[cfg(test)]
    pub keyframes_asked: AtomicUsize,
}

impl std::fmt::Debug for VideoTrack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "VideoTrack({})", self.track) }
}

impl VideoTrack {
    /// Ask the source for a keyframe now (callers coalesce with `kf_due`).
    fn ask_keyframe(&self) {
        #[cfg(test)]
        self.keyframes_asked.fetch_add(1, Ordering::Relaxed);
        (self.keyframe)(self.track);
    }

    /// Remote viewers (tests).
    #[cfg(test)]
    pub fn remote_viewers(&self) -> usize { self.inner.lock().viewers.values().filter(|v| matches!(v, Viewer::Remote(_))).count() }
}

struct Inner {
    group: u32,
    live: bool,
    cache: Vec<Wire>,
    cache_bytes: usize,
    cache_ok: bool,
    cache_at: Option<Instant>,
    first_ts: u32,
    viewers: HashMap<ViewerId, Viewer>,
    last_kf: Option<Instant>,
    aimd: Aimd,
    bytes_in: u64,
    window_at: Instant,
    rate_bps: u32,
    /// Last time each remote viewer was seeded from the cache. Kept outside the viewer, so an
    /// Unsubscribe and Subscribe toggle cannot re-send the whole cache more than once per
    /// `KEYFRAME_EVERY` (bounded: entries older than that are pruned).
    seeded: HashMap<UserId, Instant>,
}

enum Viewer {
    Remote(RemoteViewer),
    Local(LocalViewer),
}

/// Result of offering a frame to a viewer.
#[derive(Debug, PartialEq, Eq)]
enum Offer {
    Ok,
    /// Waiting for a keyframe.
    NeedKey,
    /// Dropped by the drop policy (congestion).
    Dropped,
    /// The viewer is gone.
    Closed,
}

impl Viewer {
    fn offer(&mut self, track: u16, group: u32, wire: &Wire, bps: u32) -> Offer {
        match self {
            Viewer::Remote(r) => r.offer(group, wire, bps),
            Viewer::Local(l) => l.offer(track, wire),
        }
    }
}

/// Queue state shared between a remote viewer's producer side and its writer task.
#[derive(Debug, Default)]
struct Shared {
    frames: AtomicUsize,
    bytes: AtomicUsize,
    seeding: AtomicUsize,
    flushes: AtomicU64,
    want_key: AtomicBool,
    flush: Notify,
    /// The viewer was removed or replaced: the writer stops at once (it may be parked in
    /// `open_uni` towards a peer that never grants stream credit).
    closed: AtomicBool,
}

impl Shared {
    /// Abandon the current group: reset its stream, discard what is queued, skip to the next
    /// keyframe.
    fn flush(&self) {
        self.flushes.fetch_add(1, Ordering::AcqRel);
        self.want_key.store(true, Ordering::Release);
        self.flush.notify_one();
    }
}

enum WMsg {
    Frame { flushes: u64, group: u32, wire: Wire, at: Instant, seed: bool },
    Finish,
}

/// The producer side of a remote viewer. Dropping it stops the writer task (its queued
/// frames are abandoned and its stream finished).
struct RemoteViewer {
    tx: mpsc::Sender<WMsg>,
    sh: Arc<Shared>,
    /// `Connection::stable_id` of the viewer's connection.
    stable: usize,
}

impl Drop for RemoteViewer {
    fn drop(&mut self) {
        self.sh.closed.store(true, Ordering::Release);
        self.sh.flush.notify_one();
    }
}

/// Is a backlog of `frames` / `bytes` too much for a track running at `bps`?
pub(crate) fn overloaded(frames: usize, bytes: usize, bps: u32) -> bool {
    let budget = (bps.max(VIDEO_MIN_BPS) / 8 / QUEUE_MAX_SECONDS_DIV) as usize;
    frames > QUEUE_MIN_FRAMES && bytes > budget
}

impl RemoteViewer {
    fn offer(&mut self, group: u32, wire: &Wire, bps: u32) -> Offer {
        let sh = &self.sh;
        if sh.want_key.load(Ordering::Acquire) && !wire.key {
            return Offer::NeedKey;
        }
        if !wire.key
            && sh.seeding.load(Ordering::Acquire) == 0
            && overloaded(sh.frames.load(Ordering::Acquire), sh.bytes.load(Ordering::Acquire), bps)
        {
            sh.flush();
            return Offer::Dropped;
        }
        self.enqueue(group, wire, false)
    }

    fn enqueue(&mut self, group: u32, wire: &Wire, seed: bool) -> Offer {
        let sh = &self.sh;
        sh.frames.fetch_add(1, Ordering::AcqRel);
        sh.bytes.fetch_add(wire.len(), Ordering::AcqRel);
        if seed {
            sh.seeding.fetch_add(1, Ordering::AcqRel);
        }
        let msg = WMsg::Frame { flushes: sh.flushes.load(Ordering::Acquire), group, wire: wire.clone(), at: Instant::now(), seed };
        let undo = || {
            sh.frames.fetch_sub(1, Ordering::AcqRel);
            sh.bytes.fetch_sub(wire.len(), Ordering::AcqRel);
            if seed {
                sh.seeding.fetch_sub(1, Ordering::AcqRel);
            }
        };
        match self.tx.try_send(msg) {
            Ok(()) => {
                if wire.key {
                    sh.want_key.store(false, Ordering::Release);
                }
                Offer::Ok
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                undo();
                sh.flush();
                Offer::Dropped
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                undo();
                Offer::Closed
            }
        }
    }
}

struct LocalViewer {
    out: VideoOut,
    want_key: bool,
}

impl LocalViewer {
    fn offer(&mut self, track: u16, wire: &Wire) -> Offer {
        if self.want_key && !wire.key {
            return Offer::NeedKey;
        }
        match self.out.send(track, wire.frame()) {
            Ok(()) => {
                self.want_key = false;
                Offer::Ok
            }
            Err(OutError::Full) => {
                self.want_key = true;
                Offer::Dropped
            }
            Err(OutError::Closed) => Offer::Closed,
        }
    }
}

impl VideoTrack {
    /// A track with no viewers. `keyframe` asks the source for a keyframe.
    pub fn new(track: u16, tasks: Arc<Tasks>, keyframe: KeyframeFn) -> Arc<Self> {
        let now = Instant::now();
        Arc::new(Self {
            track,
            inner: Mutex::new(Inner {
                group: 0,
                live: false,
                cache: Vec::new(),
                cache_bytes: 0,
                cache_ok: false,
                cache_at: None,
                first_ts: 0,
                viewers: HashMap::new(),
                last_kf: None,
                aimd: Aimd::new(now),
                bytes_in: 0,
                window_at: now,
                rate_bps: 0,
                seeded: HashMap::new(),
            }),
            keyframe,
            tasks,
            #[cfg(test)]
            seeds: AtomicUsize::new(0),
            #[cfg(test)]
            keyframes_asked: AtomicUsize::new(0),
        })
    }

    /// Ask the source for a keyframe (coalesced to one per second).
    pub fn request_keyframe(&self) {
        let due = {
            let mut t = self.inner.lock();
            kf_due(&mut t.last_kf, Instant::now())
        };
        if due {
            self.ask_keyframe();
        }
    }

    /// A frame from the source. A keyframe opens a new group.
    pub fn push(&self, f: VideoFrame) {
        let wire = Wire::encode(&f);
        let now = Instant::now();
        let mut need_kf = false;
        {
            let mut t = self.inner.lock();
            t.bytes_in += wire.len() as u64;
            let secs = now.saturating_duration_since(t.window_at).as_secs_f64();
            if secs >= RATE_WINDOW_S {
                t.rate_bps = (t.bytes_in as f64 * 8.0 / secs).min(f64::from(u32::MAX)) as u32;
                t.bytes_in = 0;
                t.window_at = now;
            }
            if wire.key {
                t.group = t.group.wrapping_add(1).max(1);
                t.live = true;
                t.cache.clear();
                t.cache_bytes = 0;
                t.cache_ok = true;
                t.cache_at = Some(now);
                t.first_ts = wire.ts;
            } else if !t.live {
                need_kf = kf_due(&mut t.last_kf, now);
                drop(t);
                if need_kf {
                    self.ask_keyframe();
                }
                return;
            }
            if t.cache_ok {
                if t.cache_bytes + wire.len() > CACHE_MAX_BYTES || wire.ts.wrapping_sub(t.first_ts) > CACHE_MAX_TS {
                    t.cache_ok = false;
                    t.cache.clear();
                    t.cache_bytes = 0;
                } else {
                    t.cache_bytes += wire.len();
                    t.cache.push(wire.clone());
                }
            }
            // Until a rate was measured, the target stands in for it.
            let bps = if t.rate_bps > 0 { t.rate_bps } else { t.aimd.current() };
            let (track, group) = (self.track, t.group);
            let mut closed = Vec::new();
            let mut congested = false;
            let mut wants_key = false;
            for (id, v) in t.viewers.iter_mut() {
                match v.offer(track, group, &wire, bps) {
                    Offer::Ok => {}
                    Offer::NeedKey => wants_key = true,
                    Offer::Dropped => {
                        congested = true;
                        wants_key = true;
                    }
                    Offer::Closed => closed.push(*id),
                }
            }
            for id in closed {
                t.viewers.remove(&id);
            }
            if congested {
                t.aimd.congestion();
            }
            if wants_key {
                need_kf = kf_due(&mut t.last_kf, now);
            }
        }
        if need_kf {
            self.ask_keyframe();
        }
    }

    /// Frames were lost upstream: drop deltas until the next keyframe and ask for one.
    pub fn gap(&self) {
        let due = {
            let mut t = self.inner.lock();
            t.live = false;
            t.cache_ok = false;
            t.cache.clear();
            t.cache_bytes = 0;
            t.aimd.congestion();
            kf_due(&mut t.last_kf, Instant::now())
        };
        if due {
            self.ask_keyframe();
        }
    }

    /// The source stopped: finish every viewer's group and forget the cache.
    pub fn end(&self) {
        let mut t = self.inner.lock();
        t.live = false;
        t.cache_ok = false;
        t.cache.clear();
        t.cache_bytes = 0;
        for v in t.viewers.values() {
            if let Viewer::Remote(r) = v {
                let _ = r.tx.try_send(WMsg::Finish);
            }
        }
    }

    /// Add a remote viewer reached over `conn` (replacing one on another connection). It
    /// starts with the cached group right away, or waits for the next keyframe. A viewer that
    /// already exists on the same connection is kept as it is: one writer per (viewer,
    /// track), and a repeated subscribe does not re-seed the cache. A viewer seeded less than
    /// `KEYFRAME_EVERY` ago (an Unsubscribe and Subscribe toggle) waits for the next keyframe
    /// instead of getting the cache again.
    pub fn add_remote(&self, peer: UserId, conn: Connection) {
        let stable = conn.stable_id();
        if matches!(self.inner.lock().viewers.get(&ViewerId::Peer(peer)), Some(Viewer::Remote(r)) if r.stable == stable) {
            return;
        }
        let sh = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel(VIEWER_QUEUE);
        self.tasks.spawn(writer(conn, self.track, rx, sh.clone()));
        let mut v = RemoteViewer { tx, sh, stable };
        let now = Instant::now();
        let need_kf = {
            let mut t = self.inner.lock();
            let need = if seeded_recently(&t, &peer, now) {
                v.sh.want_key.store(true, Ordering::Release);
                true
            } else {
                #[cfg(test)]
                self.seeds.fetch_add(1, Ordering::Relaxed);
                note_seed(&mut t, peer, now);
                seed(&mut t, &mut v)
            };
            t.viewers.insert(ViewerId::Peer(peer), Viewer::Remote(v));
            need && kf_due(&mut t.last_kf, now)
        };
        if need_kf {
            self.ask_keyframe();
        }
    }

    /// Add (or replace) this device's app as a viewer.
    pub fn add_local(&self, out: VideoOut) {
        let track = self.track;
        let need_kf = {
            let mut t = self.inner.lock();
            let mut l = LocalViewer { out, want_key: true };
            let fresh = t.cache_at.is_some_and(|a| a.elapsed() < CACHE_FRESH);
            let mut need = !(t.cache_ok && t.live) || !fresh;
            if t.cache_ok && t.live {
                for w in &t.cache {
                    if l.offer(track, w) != Offer::Ok {
                        need = true;
                        break;
                    }
                }
            }
            t.viewers.insert(ViewerId::Local, Viewer::Local(l));
            need && kf_due(&mut t.last_kf, Instant::now())
        };
        if need_kf {
            self.ask_keyframe();
        }
    }

    /// A viewer asked for a keyframe (decoder trouble, a gap): restart it from the cache when
    /// that is fresh (at most once per `KEYFRAME_EVERY` per viewer, the honest rate),
    /// otherwise skip it to the next keyframe and ask upstream (coalesced).
    pub fn viewer_wants_key(&self, id: ViewerId) {
        let now = Instant::now();
        let need_kf = {
            let mut t = self.inner.lock();
            let fresh = t.cache_ok && t.live && t.cache_at.is_some_and(|a| a.elapsed() < CACHE_FRESH);
            let mut need = !fresh;
            let mut v = t.viewers.remove(&id);
            if let (Some(Viewer::Remote(r)), ViewerId::Peer(peer)) = (&mut v, id) {
                let may_seed = !seeded_recently(&t, &peer, now);
                if fresh && may_seed {
                    r.sh.flush();
                    note_seed(&mut t, peer, now);
                    #[cfg(test)]
                    self.seeds.fetch_add(1, Ordering::Relaxed);
                    need |= seed(&mut t, r);
                } else if fresh {
                    // Too soon for another full re-seed: wait for the next keyframe.
                    r.sh.flush();
                    need = true;
                }
            }
            if let Some(v) = v {
                t.viewers.insert(id, v);
            }
            need && kf_due(&mut t.last_kf, now)
        };
        if need_kf {
            self.ask_keyframe();
        }
    }

    /// Remove a viewer (its stream is finished).
    pub fn remove(&self, id: ViewerId) { self.inner.lock().viewers.remove(&id); }

    /// Remove every viewer.
    pub fn clear(&self) { self.inner.lock().viewers.clear(); }

    /// Current viewers.
    pub fn viewer_ids(&self) -> Vec<ViewerId> { self.inner.lock().viewers.keys().copied().collect() }

    /// A viewer reported loss: count it as congestion for this window.
    pub fn report_loss(&self) { self.inner.lock().aimd.congestion(); }

    /// Close the rate window. Returns a new target to announce, if it moved.
    pub fn tick(&self, now: Instant, cap: u32) -> Option<u32> {
        let mut t = self.inner.lock();
        // A source that went quiet has no rate any more.
        if now.saturating_duration_since(t.window_at).as_secs_f64() >= 2.0 * RATE_WINDOW_S {
            t.rate_bps = 0;
        }
        let rate = t.rate_bps;
        t.aimd.observe(rate);
        t.aimd.tick(now, cap)
    }
}

/// Coalesce keyframe requests: true (and remember `now`) if one may go out.
fn kf_due(last: &mut Option<Instant>, now: Instant) -> bool {
    if last.is_some_and(|t| now.saturating_duration_since(t) < KEYFRAME_EVERY) {
        return false;
    }
    *last = Some(now);
    true
}

/// Was `peer` seeded from the cache less than `KEYFRAME_EVERY` ago (on this track)?
fn seeded_recently(t: &Inner, peer: &UserId, now: Instant) -> bool {
    t.seeded.get(peer).is_some_and(|at| now.saturating_duration_since(*at) < KEYFRAME_EVERY)
}

/// Remember that `peer` was seeded at `now` (entries past `KEYFRAME_EVERY` are pruned).
fn note_seed(t: &mut Inner, peer: UserId, now: Instant) {
    if t.seeded.len() >= 64 {
        t.seeded.retain(|_, at| now.saturating_duration_since(*at) < KEYFRAME_EVERY);
    }
    t.seeded.insert(peer, now);
}

/// Queue the cached group for a (re)starting viewer. True if a keyframe should be requested
/// (no usable cache, or it is old).
fn seed(t: &mut Inner, v: &mut RemoteViewer) -> bool {
    if !(t.cache_ok && t.live) || t.cache.is_empty() {
        v.sh.want_key.store(true, Ordering::Release);
        return true;
    }
    for w in &t.cache {
        if v.enqueue(t.group, w, true) != Offer::Ok {
            return true;
        }
    }
    t.cache_at.is_none_or(|a| a.elapsed() >= CACHE_FRESH)
}

/// Outcome of a write that a flush may interrupt.
enum Wrote {
    Done,
    Flushed,
    /// The viewer stopped this stream.
    Stopped,
    /// The connection is gone.
    Lost,
}

/// Write `bufs`, giving up when a flush (generation change) arrives meanwhile.
async fn write_or_flush(s: &mut SendStream, bufs: &mut [Bytes], sh: &Shared, seen: u64) -> Wrote {
    let fut = s.write_all_chunks(bufs);
    tokio::pin!(fut);
    loop {
        tokio::select! {
            biased;
            _ = sh.flush.notified() => {
                if sh.closed.load(Ordering::Acquire) || sh.flushes.load(Ordering::Acquire) != seen {
                    return Wrote::Flushed;
                }
            }
            r = &mut fut => {
                return match r {
                    Ok(()) => Wrote::Done,
                    Err(WriteError::ConnectionLost(_)) => Wrote::Lost,
                    Err(_) => Wrote::Stopped,
                };
            }
        }
    }
}

fn reset(cur: &mut Option<(u32, SendStream)>) {
    if let Some((_, mut s)) = cur.take() {
        let _ = s.reset(VarInt::from_u32(VIDEO_RESET));
    }
}

/// Writer task of one remote viewer: one uni stream per group.
async fn writer(conn: Connection, track: u16, mut rx: mpsc::Receiver<WMsg>, sh: Arc<Shared>) {
    let mut seen = sh.flushes.load(Ordering::Acquire);
    let mut cur: Option<(u32, SendStream)> = None;
    loop {
        if sh.closed.load(Ordering::Acquire) {
            reset(&mut cur);
            break;
        }
        let msg = tokio::select! {
            biased;
            _ = sh.flush.notified() => {
                let g = sh.flushes.load(Ordering::Acquire);
                if g != seen {
                    seen = g;
                    reset(&mut cur);
                }
                continue;
            }
            m = rx.recv() => match m {
                Some(m) => m,
                None => break,
            },
        };
        let (flushes, group, wire, at, is_seed) = match msg {
            WMsg::Finish => {
                if let Some((_, mut s)) = cur.take() {
                    let _ = s.finish();
                }
                continue;
            }
            WMsg::Frame { flushes, group, wire, at, seed } => (flushes, group, wire, at, seed),
        };
        sh.frames.fetch_sub(1, Ordering::AcqRel);
        sh.bytes.fetch_sub(wire.len(), Ordering::AcqRel);
        if is_seed {
            sh.seeding.fetch_sub(1, Ordering::AcqRel);
        }
        let g = sh.flushes.load(Ordering::Acquire);
        if g != seen {
            seen = g;
            reset(&mut cur);
        }
        if flushes < seen {
            continue;
        }
        if !is_seed && at.elapsed() > STALE {
            sh.flush();
            seen = sh.flushes.load(Ordering::Acquire);
            reset(&mut cur);
            continue;
        }
        let mut bufs = vec![wire.bytes.clone()];
        if wire.key {
            if let Some((_, mut old)) = cur.take() {
                let _ = old.set_priority(PRIO_OLD);
                let _ = old.finish();
            }
            let opened = {
                // A peer that never grants stream credit must not park this task forever.
                let fut = tokio::time::timeout(OPEN_TIMEOUT, conn.open_uni());
                tokio::pin!(fut);
                loop {
                    tokio::select! {
                        biased;
                        _ = sh.flush.notified() => {
                            if sh.closed.load(Ordering::Acquire) || sh.flushes.load(Ordering::Acquire) != seen {
                                break None;
                            }
                        }
                        r = &mut fut => break Some(r),
                    }
                }
            };
            let s = match opened {
                None => {
                    seen = sh.flushes.load(Ordering::Acquire);
                    continue;
                }
                Some(Ok(Ok(s))) => s,
                Some(Ok(Err(_))) => break,
                Some(Err(_)) => {
                    // No stream within the timeout: skip this group, wait for the next key.
                    sh.flush();
                    seen = sh.flushes.load(Ordering::Acquire);
                    continue;
                }
            };
            let _ = s.set_priority(PRIO_NEW);
            let header = VideoStreamHeader { track, group_seq: group, codec: CODEC_H264_ANNEXB, flags: 0 }.write();
            bufs.insert(0, Bytes::copy_from_slice(&header));
            cur = Some((group, s));
        } else if !matches!(&cur, Some((g, _)) if *g == group) {
            continue;
        }
        let Some((_, s)) = cur.as_mut() else { continue };
        match write_or_flush(s, &mut bufs, &sh, seen).await {
            Wrote::Done => {}
            Wrote::Flushed => {
                seen = sh.flushes.load(Ordering::Acquire);
                reset(&mut cur);
            }
            Wrote::Stopped => cur = None,
            Wrote::Lost => break,
        }
    }
    if let Some((_, mut s)) = cur.take() {
        let _ = s.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overload_rule_needs_frames_and_time() {
        // 2 Mbps: 250 ms = 62.5 KB.
        assert!(!overloaded(2, 1_000_000, 2_000_000), "two frames are always fine");
        assert!(!overloaded(10, 60_000, 2_000_000), "less than 250 ms queued");
        assert!(overloaded(3, 70_000, 2_000_000));
        // Unknown rate: the floor (300 kbps, about 9.4 KB) applies.
        assert!(overloaded(3, 10_000, 0));
        assert!(!overloaded(3, 9_000, 0));
    }

    #[test]
    fn keyframe_requests_are_coalesced() {
        let t0 = Instant::now();
        let mut last = None;
        assert!(kf_due(&mut last, t0));
        assert!(!kf_due(&mut last, t0 + Duration::from_millis(500)));
        assert!(kf_due(&mut last, t0 + Duration::from_millis(1000)));
        assert!(!kf_due(&mut last, t0 + Duration::from_millis(1999)));
    }

    fn frame(ts: u32, key: bool, n: usize) -> VideoFrame { VideoFrame { ts, keyframe: key, data: Bytes::from(vec![7u8; n]) } }

    #[tokio::test]
    async fn local_viewer_skips_to_keyframes_and_cache_seeds() {
        let asked = Arc::new(AtomicUsize::new(0));
        let a = asked.clone();
        let t = VideoTrack::new(5, Arc::new(Tasks::default()), Box::new(move |_| {
            a.fetch_add(1, Ordering::SeqCst);
        }));
        // Deltas before any keyframe: dropped, one coalesced request.
        t.push(frame(0, false, 10));
        t.push(frame(1, false, 10));
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        // A group arrives with nobody watching: it is cached.
        t.push(frame(3000, true, 100));
        t.push(frame(6000, false, 10));
        let (tx, mut rx) = mpsc::channel(4);
        t.add_local(VideoOut::new(tx));
        let (tr, f) = rx.try_recv().unwrap();
        assert_eq!((tr, f.keyframe, f.ts), (5, true, 3000));
        assert_eq!(rx.try_recv().unwrap().1.ts, 6000);
        // Live frames follow.
        t.push(frame(9000, false, 10));
        assert_eq!(rx.try_recv().unwrap().1.ts, 9000);
        // The app stops reading: the queue fills, the viewer waits for the next keyframe.
        for i in 0..6 {
            t.push(frame(12_000 + i, false, 10));
        }
        while rx.try_recv().is_ok() {}
        t.push(frame(20_000, false, 10));
        assert!(rx.try_recv().is_err(), "deltas after a drop are skipped");
        t.push(frame(30_000, true, 10));
        let (_, k) = rx.try_recv().unwrap();
        assert!(k.keyframe);
        // A gap upstream: deltas stop until the next keyframe.
        t.gap();
        t.push(frame(33_000, false, 10));
        assert!(rx.try_recv().is_err());
        t.remove(ViewerId::Local);
        assert!(t.viewer_ids().is_empty());
    }

    #[tokio::test]
    async fn cache_is_bounded() {
        let t = VideoTrack::new(5, Arc::new(Tasks::default()), Box::new(|_| {}));
        t.push(frame(0, true, 1000));
        t.push(frame(CACHE_MAX_TS + 1, false, 10));
        assert!(!t.inner.lock().cache_ok, "a group longer than 4 s is not cached");
        t.push(frame(0, true, CACHE_MAX_BYTES + 1));
        assert!(t.inner.lock().cache.is_empty(), "nor one larger than 2 MB");
    }
}
