//! Receiving media on one connection: audio datagrams for a session, and GOP streams put back
//! in group order.
//!
//! Each uni stream is read by its own task (QUIC delivers streams independently), and all of
//! them feed one ordering task per connection. Per track it delivers one group at a time, in
//! `group_seq` order, starting at a keyframe: frames of a newer group are held back until the
//! current group's stream ends, or for at most 200 ms (then the current group is abandoned).
//! Older groups are dropped. A reset or a skipped group is a gap: the sink asks for a keyframe.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::endpoint::{Connection, RecvStream, VarInt};
use tincan_core::media::{
    AudioHeader, CODEC_H264_ANNEXB, KIND_SHARE_AUDIO, VIDEO_STREAM_HEADER_LEN, VideoFrame, VideoFrameReader, VideoStreamHeader,
    is_audio_kind, screen_of_share_audio, track_kind, track_short_id,
};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;

use super::stats::RxStats;
use crate::link::Tasks;

/// Bytes per stream read.
const READ_CHUNK: usize = 64 * 1024;
/// A stream must deliver its header within this.
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
/// A newer group waits at most this long for the current one to end.
const SWITCH_AFTER: Duration = Duration::from_millis(200);
/// ...or at most this many frames...
const PENDING_MAX: usize = 60;
/// ...or this many bytes.
const PENDING_MAX_BYTES: usize = 2 * 1024 * 1024;
/// Groups of one track waiting behind the current one (honest: one, maybe two).
const MAX_PENDING_GROUPS: usize = 4;
/// Group streams read at once per connection (honest: one or two per track).
const MAX_READERS: usize = 8;
/// A frame that has started must be complete within this.
const FRAME_DEADLINE: Duration = Duration::from_secs(10);
/// Stop code for a stream we refuse (wrong track or codec).
const VIDEO_REFUSED: u32 = 2;
/// Datagrams taken per read.
const DATAGRAM_BATCH: usize = 32;

/// Where frames received on a connection go.
pub(crate) trait VideoSink: Send + Sync + 'static {
    /// May frames of `track` come in on this connection?
    fn accept(&self, track: u16) -> bool;
    /// The next frame of `track` in order. False if it could not be handed on (the track then
    /// waits for its next keyframe).
    fn frame(&self, track: u16, f: VideoFrame) -> bool;
    /// Frames of `track` were lost: ask for a keyframe.
    fn gap(&self, track: u16);
}

/// Test hook: hold every stream reader (a viewer that stops reading, so QUIC flow control
/// pushes back on the sender). Outside tests it never blocks.
#[derive(Clone, Debug)]
pub(crate) struct Pause {
    #[cfg(test)]
    rx: tokio::sync::watch::Receiver<bool>,
}

impl Pause {
    /// A pause handle following `rx` (tests) or none.
    #[cfg(test)]
    pub fn new(rx: tokio::sync::watch::Receiver<bool>) -> Self { Self { rx } }

    /// A pause handle that never pauses.
    #[cfg(not(test))]
    pub fn new() -> Self { Self {} }

    /// Wait while paused. True if it had to wait.
    async fn wait(&mut self) -> bool {
        #[cfg(test)]
        {
            let paused = *self.rx.borrow();
            let _ = self.rx.wait_for(|paused| !*paused).await;
            paused
        }
        #[cfg(not(test))]
        {
            false
        }
    }
}

enum Msg {
    Frame { track: u16, group: u32, frame: VideoFrame },
    End { track: u16, group: u32, reset: bool },
}

/// Start receiving GOP streams on `conn` into `sink`. Abort the handle to stop.
pub(crate) fn spawn_video_rx(tasks: &Arc<Tasks>, conn: Connection, sink: Arc<dyn VideoSink>, pause: Pause) -> tokio::task::AbortHandle {
    tasks.spawn(video_rx(conn, sink, pause))
}

async fn video_rx(conn: Connection, sink: Arc<dyn VideoSink>, pause: Pause) {
    let (tx, mut rx) = mpsc::channel::<Msg>(64);
    let mut readers: JoinSet<()> = JoinSet::new();
    let mut order: HashMap<u16, Order> = HashMap::new();
    loop {
        tokio::select! {
            s = conn.accept_uni() => match s {
                Ok(mut recv) => {
                    if readers.len() >= MAX_READERS {
                        // Honest senders have one or two group streams open per track.
                        let _ = recv.stop(VarInt::from_u32(VIDEO_REFUSED));
                    } else {
                        readers.spawn(read_stream(recv, sink.clone(), tx.clone(), pause.clone()));
                    }
                }
                Err(_) => break,
            },
            Some(m) = rx.recv() => on_msg(&mut order, sink.as_ref(), m),
            Some(_) = readers.join_next(), if !readers.is_empty() => {}
        }
    }
}

/// Read one chunk; once a frame has started, its rest must arrive by `deadline`.
async fn read_chunk_by(recv: &mut RecvStream, deadline: Option<Instant>) -> Result<Option<Bytes>, ()> {
    let fut = recv.read_chunk(READ_CHUNK);
    let res = match deadline {
        Some(d) => tokio::time::timeout_at(d, fut).await.map_err(|_| ())?,
        None => fut.await,
    };
    res.map_err(|_| ())
}

/// Read one GOP stream: header, then frames, then the end.
async fn read_stream(mut recv: RecvStream, sink: Arc<dyn VideoSink>, tx: mpsc::Sender<Msg>, mut pause: Pause) {
    let mut head = Vec::with_capacity(VIDEO_STREAM_HEADER_LEN);
    let got = tokio::time::timeout(HEADER_TIMEOUT, async {
        while head.len() < VIDEO_STREAM_HEADER_LEN {
            match recv.read_chunk(VIDEO_STREAM_HEADER_LEN - head.len()).await {
                Ok(Some(b)) => head.extend_from_slice(&b),
                _ => return false,
            }
        }
        true
    })
    .await;
    let header = match got {
        Ok(true) => VideoStreamHeader::parse(&head),
        _ => None,
    };
    let Some(h) = header.filter(|h| h.codec == CODEC_H264_ANNEXB && sink.accept(h.track)) else {
        let _ = recv.stop(VarInt::from_u32(VIDEO_REFUSED));
        return;
    };
    let (track, group) = (h.track, h.group_seq);
    let mut rd = VideoFrameReader::new();
    // When the frame now being read started (a partial frame must complete in time).
    let mut frame_started: Option<Instant> = None;
    loop {
        if pause.wait().await {
            // Time spent paused (tests) does not count against the frame.
            frame_started = frame_started.map(|_| Instant::now());
        }
        let deadline = frame_started.map(|t| t + FRAME_DEADLINE);
        let reset = match read_chunk_by(&mut recv, deadline).await {
            Err(()) if frame_started.is_some() => {
                let _ = recv.stop(VarInt::from_u32(VIDEO_REFUSED));
                true
            }
            Ok(Some(chunk)) => {
                rd.push(&chunk);
                let mut bad = false;
                let mut completed = false;
                loop {
                    match rd.next() {
                        Ok(Some(frame)) => {
                            completed = true;
                            if tx.send(Msg::Frame { track, group, frame }).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(()) => {
                            bad = true;
                            break;
                        }
                    }
                }
                if !bad {
                    frame_started = match (rd.mid_frame(), completed, frame_started) {
                        (false, _, _) => None,
                        (true, false, Some(t)) => Some(t),
                        (true, _, _) => Some(Instant::now()),
                    };
                    continue;
                }
                let _ = recv.stop(VarInt::from_u32(VIDEO_REFUSED));
                true
            }
            Ok(None) => false,
            Err(()) => true,
        };
        let _ = tx.send(Msg::End { track, group, reset }).await;
        return;
    }
}

/// `a` is an older group than `b` (wrapping).
fn older(a: u32, b: u32) -> bool { (a.wrapping_sub(b) as i32) < 0 }

#[derive(Debug)]
struct Pending {
    frames: VecDeque<VideoFrame>,
    bytes: usize,
    ended: bool,
    reset: bool,
    since: Instant,
}

/// Per-track ordering state.
#[derive(Debug, Default)]
struct Order {
    cur: Option<u32>,
    cur_ended: bool,
    want_key: bool,
    pending: BTreeMap<u32, Pending>,
}

impl Order {
    fn deliver(&mut self, sink: &dyn VideoSink, track: u16, f: VideoFrame) {
        if self.want_key && !f.keyframe {
            return;
        }
        if sink.frame(track, f) {
            self.want_key = false;
        } else {
            self.want_key = true;
            sink.gap(track);
        }
    }

    /// The oldest pending group newer than the current one.
    fn next_pending(&self) -> Option<u32> {
        let cur = self.cur?;
        self.pending.keys().copied().filter(|g| older(cur, *g)).min_by(|a, b| if older(*a, *b) { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater })
    }

    /// Make `group` current: deliver what it buffered, drop older pending groups. When that
    /// group had already ended, go on with the next pending one, and so on (a loop: the
    /// chain of finished pending groups is never recursed through).
    fn switch(&mut self, sink: &dyn VideoSink, track: u16, group: u32) {
        let mut next = Some(group);
        while let Some(group) = next.take() {
            // Abandoning an unfinished group loses frames, unless it was already waiting for a
            // keyframe (that gap was reported then).
            let skipped_unfinished = self.cur.is_some() && !self.cur_ended && !self.want_key;
            self.cur = Some(group);
            self.cur_ended = false;
            self.want_key = true;
            let stale: Vec<u32> = self.pending.keys().copied().filter(|g| older(*g, group)).collect();
            let mut lost = skipped_unfinished;
            for g in stale {
                self.pending.remove(&g);
                lost = true;
            }
            if lost {
                sink.gap(track);
            }
            if let Some(p) = self.pending.remove(&group) {
                for f in p.frames {
                    self.deliver(sink, track, f);
                }
                if p.ended {
                    self.cur_ended = true;
                    match self.next_pending() {
                        Some(n) => next = Some(n),
                        None if p.reset => {
                            self.want_key = true;
                            sink.gap(track);
                        }
                        None => {}
                    }
                }
            }
        }
    }

    /// The current group's stream ended.
    fn end_current(&mut self, sink: &dyn VideoSink, track: u16, reset: bool) {
        self.cur_ended = true;
        match self.next_pending() {
            Some(next) => self.switch(sink, track, next),
            None if reset => {
                self.want_key = true;
                sink.gap(track);
            }
            None => {}
        }
    }

    /// The current group is unfinished and a pending group waited too long: give up on it.
    fn check_stall(&mut self, sink: &dyn VideoSink, track: u16) {
        if self.cur.is_none() || self.cur_ended {
            return;
        }
        let stalled = self.pending.values().any(|p| p.since.elapsed() >= SWITCH_AFTER);
        if stalled && let Some(next) = self.next_pending() {
            self.switch(sink, track, next);
        }
    }
}

fn on_msg(orders: &mut HashMap<u16, Order>, sink: &dyn VideoSink, m: Msg) {
    match m {
        Msg::Frame { track, group, frame } => {
            let o = orders.entry(track).or_default();
            // The oldest waiting group's age is checked on every message, not only for the
            // group that just got a frame.
            o.check_stall(sink, track);
            // At most two rounds: a new group arriving while the pending list is full first
            // gives up on the current group (which shortens the list).
            for _ in 0..2 {
                match o.cur {
                    Some(c) if c == group => return o.deliver(sink, track, frame),
                    Some(c) if older(group, c) => return,
                    _ if o.cur.is_none() || o.cur_ended => {
                        o.switch(sink, track, group);
                        return o.deliver(sink, track, frame);
                    }
                    _ if !o.pending.contains_key(&group) && o.pending.len() >= MAX_PENDING_GROUPS => {
                        match o.next_pending() {
                            Some(next) => o.switch(sink, track, next),
                            None => return,
                        }
                    }
                    _ => {
                        // Waiting for a keyframe in the current group: a newer group is better.
                        let useless = o.want_key;
                        let p = o.pending.entry(group).or_insert_with(|| Pending {
                            frames: VecDeque::new(),
                            bytes: 0,
                            ended: false,
                            reset: false,
                            since: Instant::now(),
                        });
                        p.bytes += frame.data.len();
                        p.frames.push_back(frame);
                        let full = p.frames.len() > PENDING_MAX || p.bytes > PENDING_MAX_BYTES;
                        if useless || full || p.since.elapsed() >= SWITCH_AFTER {
                            // The current group stalled (or is useless): give up on it.
                            if let Some(next) = o.next_pending() {
                                o.switch(sink, track, next);
                            }
                        }
                        return;
                    }
                }
            }
        }
        Msg::End { track, group, reset } => {
            let Some(o) = orders.get_mut(&track) else { return };
            if o.cur == Some(group) {
                o.end_current(sink, track, reset);
            } else if let Some(p) = o.pending.get_mut(&group) {
                p.ended = true;
                p.reset = reset;
            }
            o.check_stall(sink, track);
        }
    }
}

/// Read audio datagrams on `conn` into `audio_in`: voice and soundboard tracks of anyone but
/// the owner of `own`, my voice track (counted in `stats`), and share audio only of the screen
/// tracks in `subs` (what I watch). A host or peer that sends share audio nobody asked for is
/// dropped here, so unwatching (and a minimized window, which unwatches) stops share audio even
/// if the sender keeps going. Ends with the connection.
pub(crate) async fn audio_rx(
    conn: Connection,
    own: Arc<std::sync::atomic::AtomicU16>,
    audio_in: mpsc::Sender<Bytes>,
    stats: Arc<RxStats>,
    subs: Arc<parking_lot::Mutex<std::collections::HashSet<u16>>>,
) {
    let mut buf = vec![Bytes::new(); DATAGRAM_BATCH];
    loop {
        let n = match conn.read_many_datagrams(&mut buf).await {
            Ok(n) => n,
            Err(_) => break,
        };
        let now = Instant::now();
        let own = own.load(std::sync::atomic::Ordering::Relaxed);
        for pkt in buf.iter_mut().take(n).map(std::mem::take) {
            let Some((h, _)) = AudioHeader::parse(&pkt) else { continue };
            if track_kind(h.track) == KIND_SHARE_AUDIO {
                if screen_of_share_audio(h.track).is_some_and(|s| subs.lock().contains(&s)) {
                    let _ = audio_in.try_send(pkt);
                }
                continue;
            }
            if !is_audio_kind(track_kind(h.track)) || track_short_id(h.track) == track_short_id(own) {
                continue;
            }
            stats.with(h.track, |t| t.audio(h.seq, h.ts, now));
            let _ = audio_in.try_send(pkt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct Collect {
        got: Mutex<Vec<(u16, u32, bool)>>,
        gaps: Mutex<u32>,
    }

    impl VideoSink for Collect {
        fn accept(&self, _: u16) -> bool { true }
        fn frame(&self, track: u16, f: VideoFrame) -> bool {
            self.got.lock().push((track, f.ts, f.keyframe));
            true
        }
        fn gap(&self, _: u16) { *self.gaps.lock() += 1; }
    }

    fn fr(track: u16, group: u32, ts: u32, key: bool) -> Msg {
        Msg::Frame { track, group, frame: VideoFrame { ts, keyframe: key, data: Bytes::new() } }
    }

    #[test]
    fn groups_are_delivered_in_order_without_loss() {
        let sink = Collect::default();
        let mut o = HashMap::new();
        // Group 1 starts; group 2's keyframe races ahead of group 1's tail.
        on_msg(&mut o, &sink, fr(5, 1, 0, true));
        on_msg(&mut o, &sink, fr(5, 1, 1, false));
        on_msg(&mut o, &sink, fr(5, 2, 3, true));
        on_msg(&mut o, &sink, fr(5, 1, 2, false));
        on_msg(&mut o, &sink, Msg::End { track: 5, group: 1, reset: false });
        on_msg(&mut o, &sink, fr(5, 2, 4, false));
        let ts: Vec<u32> = sink.got.lock().iter().map(|(_, t, _)| *t).collect();
        assert_eq!(ts, vec![0, 1, 2, 3, 4]);
        assert_eq!(*sink.gaps.lock(), 0);
        // An old group's late frame is ignored.
        on_msg(&mut o, &sink, fr(5, 1, 99, false));
        assert_eq!(sink.got.lock().len(), 5);
    }

    #[test]
    fn reset_waits_for_the_next_keyframe() {
        let sink = Collect::default();
        let mut o = HashMap::new();
        on_msg(&mut o, &sink, fr(9, 7, 0, true));
        on_msg(&mut o, &sink, Msg::End { track: 9, group: 7, reset: true });
        assert_eq!(*sink.gaps.lock(), 1, "a reset asks for a keyframe");
        // A group that does not start with a keyframe is skipped until one comes.
        on_msg(&mut o, &sink, fr(9, 8, 10, false));
        on_msg(&mut o, &sink, fr(9, 8, 11, false));
        on_msg(&mut o, &sink, fr(9, 9, 20, true));
        let got: Vec<(u32, bool)> = sink.got.lock().iter().map(|(_, t, k)| (*t, *k)).collect();
        assert_eq!(got, vec![(0, true), (20, true)]);
    }

    #[test]
    fn a_stalled_group_is_abandoned() {
        let sink = Collect::default();
        let mut o = HashMap::new();
        on_msg(&mut o, &sink, fr(1, 1, 0, true));
        for i in 0..=PENDING_MAX as u32 {
            on_msg(&mut o, &sink, fr(1, 2, 100 + i, i == 0));
        }
        let got = sink.got.lock().clone();
        assert_eq!(got.first().map(|g| g.1), Some(0));
        assert_eq!(got.get(1).map(|g| (g.1, g.2)), Some((100, true)), "switched to the newer group");
        assert_eq!(*sink.gaps.lock(), 1, "abandoning the stalled group counts as a gap");
        // Wrapping group numbers still order correctly.
        assert!(older(u32::MAX, 0));
        assert!(!older(0, u32::MAX));
    }

    #[test]
    fn many_new_groups_behind_an_open_one_stay_bounded_and_drain_without_recursion() {
        // One open group, then 20k tiny finished groups: pending used to grow without
        // bound and finishing the first stream recursed once per group (stack overflow).
        let sink = Collect::default();
        let mut o = HashMap::new();
        on_msg(&mut o, &sink, fr(5, 1, 0, true));
        for g in 2..20_002u32 {
            on_msg(&mut o, &sink, fr(5, g, g, true));
            on_msg(&mut o, &sink, Msg::End { track: 5, group: g, reset: false });
            assert!(o[&5].pending.len() <= 4, "pending grew to {}", o[&5].pending.len());
        }
        on_msg(&mut o, &sink, Msg::End { track: 5, group: 1, reset: false });
        assert!(o[&5].pending.len() <= 4);
        // Run on a small stack: a recursive drain of even a few thousand groups would not fit.
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let sink = Collect::default();
                let mut o = HashMap::new();
                on_msg(&mut o, &sink, fr(7, 1, 0, true));
                for g in 2..5_000u32 {
                    on_msg(&mut o, &sink, fr(7, g, g, true));
                    on_msg(&mut o, &sink, Msg::End { track: 7, group: g, reset: false });
                }
                on_msg(&mut o, &sink, Msg::End { track: 7, group: 1, reset: false });
                sink.got.lock().len()
            })
            .unwrap();
        assert!(handle.join().unwrap() > 0);
    }

    #[test]
    fn a_pending_group_is_bounded_in_bytes() {
        let sink = Collect::default();
        let mut o = HashMap::new();
        on_msg(&mut o, &sink, fr(3, 1, 0, true));
        // Group 2 waits behind the open group 1; big frames make it switch early.
        on_msg(&mut o, &sink, Msg::Frame { track: 3, group: 2, frame: VideoFrame { ts: 1, keyframe: true, data: Bytes::from(vec![0u8; PENDING_MAX_BYTES]) } });
        assert!(o[&3].pending.contains_key(&2));
        on_msg(&mut o, &sink, Msg::Frame { track: 3, group: 2, frame: VideoFrame { ts: 2, keyframe: false, data: Bytes::from(vec![0u8; 10]) } });
        assert_eq!(o[&3].cur, Some(2), "switched instead of buffering more");
        assert!(o[&3].pending.is_empty());
    }
}
