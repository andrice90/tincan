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
    AudioHeader, CODEC_H264_ANNEXB, KIND_VOICE, VIDEO_STREAM_HEADER_LEN, VideoFrame, VideoFrameReader, VideoStreamHeader, track_kind,
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
/// ...or at most this many frames.
const PENDING_MAX: usize = 60;
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

    async fn wait(&mut self) {
        #[cfg(test)]
        {
            let _ = self.rx.wait_for(|paused| !*paused).await;
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
                Ok(recv) => {
                    readers.spawn(read_stream(recv, sink.clone(), tx.clone(), pause.clone()));
                }
                Err(_) => break,
            },
            Some(m) = rx.recv() => on_msg(&mut order, sink.as_ref(), m),
            Some(_) = readers.join_next(), if !readers.is_empty() => {}
        }
    }
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
    loop {
        pause.wait().await;
        let reset = match recv.read_chunk(READ_CHUNK).await {
            Ok(Some(chunk)) => {
                rd.push(&chunk);
                let mut bad = false;
                loop {
                    match rd.next() {
                        Ok(Some(frame)) => {
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
                    continue;
                }
                let _ = recv.stop(VarInt::from_u32(VIDEO_REFUSED));
                true
            }
            Ok(None) => false,
            Err(_) => true,
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

    /// Make `group` current: deliver what it buffered, drop older pending groups.
    fn switch(&mut self, sink: &dyn VideoSink, track: u16, group: u32) {
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
                self.end_current(sink, track, p.reset);
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
}

fn on_msg(orders: &mut HashMap<u16, Order>, sink: &dyn VideoSink, m: Msg) {
    match m {
        Msg::Frame { track, group, frame } => {
            let o = orders.entry(track).or_default();
            match o.cur {
                Some(c) if c == group => o.deliver(sink, track, frame),
                Some(c) if older(group, c) => {}
                _ if o.cur.is_none() || o.cur_ended => {
                    o.switch(sink, track, group);
                    o.deliver(sink, track, frame);
                }
                _ => {
                    // Waiting for a keyframe in the current group: a newer group is better.
                    let useless = o.want_key;
                    let p = o.pending.entry(group).or_insert_with(|| Pending { frames: VecDeque::new(), ended: false, reset: false, since: Instant::now() });
                    p.frames.push_back(frame);
                    if useless || p.since.elapsed() >= SWITCH_AFTER || p.frames.len() > PENDING_MAX {
                        // The current group stalled (or is useless): give up on it.
                        if let Some(next) = o.next_pending() {
                            o.switch(sink, track, next);
                        }
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
        }
    }
}

/// Read audio datagrams on `conn` into `audio_in` (voice tracks other than `own`), counting
/// them in `stats`. Ends with the connection.
pub(crate) async fn audio_rx(conn: Connection, own: Arc<std::sync::atomic::AtomicU16>, audio_in: mpsc::Sender<Bytes>, stats: Arc<RxStats>) {
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
            if track_kind(h.track) != KIND_VOICE || h.track == own {
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
}
