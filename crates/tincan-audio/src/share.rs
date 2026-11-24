//! Screen share audio: capture, Opus stereo encode and packetizing, on a thread of its own.
//!
//! [`ShareAudio::start`] opens a source and runs the `tincan-share-audio` thread until
//! [`ShareAudio::stop`] (or drop). Every 20 ms of 48 kHz stereo becomes one audio datagram
//! (`tincan_core::media::AudioHeader` + Opus, music application, 128 kbps VBR, no DTX, no
//! FEC). The header's track and epoch are left at 0: the network layer stamps the sender's
//! share-audio track (`KIND_SHARE_AUDIO`) and the host epoch. `AF_SPEAKING` is never set, so a
//! receiver never lights the sharer's speaking ring from it.
//!
//! The stream follows the wall clock: when the source delivers less than real time (process
//! loopback delivers nothing while the target is silent), the gap is filled with silence, so
//! timestamps and sequence numbers advance at 50 packets a second and a receiver's jitter
//! buffer never sees a late burst. A source that delivers more is trimmed to at most
//! [`MAX_BACKLOG_MS`] of buffered audio.
//!
//! Nothing here touches the audio engine thread: received share audio is played by the engine
//! like any other track (see `Processor`'s share streams).

use crate::codec::{MAX_OPUS_PACKET, OpusCodec, OpusEncode, StereoSettings};
use crate::dsp::{level_byte, rms};
use crate::error::{AudioError, Result};
use crate::LibOpus;
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tincan_core::media::{AF_TALKSPURT, AudioHeader};
use tokio::sync::mpsc;

/// Samples per channel in one share-audio packet (20 ms at 48 kHz).
pub const SHARE_FRAME: usize = 960;
/// Largest Opus payload of a share-audio packet. With the 12-byte header this stays well
/// inside one QUIC datagram on every path; 128 kbps VBR needs about 320 bytes.
pub const SHARE_MAX_PAYLOAD: usize = 1000;
/// Most audio kept waiting before it is encoded; anything older is dropped.
pub const MAX_BACKLOG_MS: u64 = 100;
/// How often the capture thread wakes.
const POLL: Duration = Duration::from_millis(10);
/// The source may fall this far behind the wall clock before silence fills the gap.
const FILL_AFTER_MS: u64 = 40;
/// How long `start` waits for the source to open.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// What a share captures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareAudioSource {
    /// Audio of this process and every process it started (a shared window's app).
    /// Windows process loopback, `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`.
    ProcessTree(u32),
    /// Everything the PC plays except this process tree (a shared display, excluding
    /// Tincan itself). `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE`.
    AllExcept(u32),
    /// A synthetic stereo tone at real-time pace (tests and `no_devices` profiles).
    TestTone,
}

/// Something that yields 48 kHz stereo interleaved samples.
pub(crate) trait StereoSource {
    /// Appends what is available now (interleaved L, R).
    fn read(&mut self, out: &mut Vec<f32>) -> Result<()>;
}

/// A 220 Hz tone on the left, 330 Hz on the right, paced by the wall clock.
struct ToneSource {
    start: Instant,
    produced: u64,
}

impl ToneSource {
    fn new() -> Self {
        Self { start: Instant::now(), produced: 0 }
    }
}

impl StereoSource for ToneSource {
    fn read(&mut self, out: &mut Vec<f32>) -> Result<()> {
        let due = self.start.elapsed().as_micros() as u64 * 48 / 1000;
        let tau = 2.0 * std::f32::consts::PI;
        while self.produced < due {
            let t = self.produced as f32 / 48_000.0;
            out.push(0.2 * (tau * 220.0 * t).sin());
            out.push(0.2 * (tau * 330.0 * t).sin());
            self.produced += 1;
        }
        Ok(())
    }
}

/// Turns stereo samples into share-audio datagrams.
pub(crate) struct Packetizer {
    enc: Box<dyn OpusEncode>,
    pcm: Vec<f32>,
    pkt: Vec<u8>,
    seq: u16,
    ts: u32,
    first: bool,
    /// Samples per channel encoded or waiting since the start.
    taken: u64,
    out: mpsc::Sender<Bytes>,
    sent: Arc<AtomicU64>,
}

impl Packetizer {
    pub(crate) fn new(codec: &dyn OpusCodec, out: mpsc::Sender<Bytes>, sent: Arc<AtomicU64>) -> Result<Self> {
        Ok(Self {
            enc: codec.stereo_encoder(&StereoSettings::default())?,
            pcm: Vec::with_capacity(SHARE_FRAME * 2 * 8),
            pkt: vec![0; SHARE_MAX_PAYLOAD.min(MAX_OPUS_PACKET)],
            seq: rand::random(),
            ts: rand::random(),
            first: true,
            taken: 0,
            out,
            sent,
        })
    }

    /// Adds interleaved samples and sends every complete 20 ms frame. False once the receiver
    /// is gone (the share ended).
    pub(crate) fn push(&mut self, samples: &[f32]) -> bool {
        let max = (MAX_BACKLOG_MS as usize * 48 + SHARE_FRAME) * 2;
        self.pcm.extend_from_slice(samples);
        if self.pcm.len() > max {
            let cut = (self.pcm.len() - max) & !1;
            self.pcm.drain(..cut);
        }
        self.taken += (samples.len() / 2) as u64;
        while self.pcm.len() >= SHARE_FRAME * 2 {
            if !self.encode() {
                return false;
            }
        }
        true
    }

    /// Fills silence until `due` samples per channel have been taken, keeping
    /// `FILL_AFTER_MS` of slack for a source that is just late.
    pub(crate) fn fill_to(&mut self, due: u64) -> bool {
        let slack = FILL_AFTER_MS * 48;
        if self.taken + slack >= due {
            return true;
        }
        let missing = (due - self.taken - slack / 2) as usize;
        let zeros = vec![0f32; missing * 2];
        self.push(&zeros)
    }

    fn encode(&mut self) -> bool {
        let frame: Vec<f32> = self.pcm.drain(..SHARE_FRAME * 2).collect();
        let ts = self.ts;
        self.ts = self.ts.wrapping_add(SHARE_FRAME as u32);
        let n = match self.enc.encode(&frame, &mut self.pkt) {
            Ok(n) if n > 2 => n,
            Ok(_) => return true,
            Err(e) => {
                tracing::debug!(%e, "share audio encode");
                return true;
            }
        };
        let flags = if self.first { AF_TALKSPURT } else { 0 };
        self.first = false;
        let header = AudioHeader { flags, track: 0, seq: self.seq, ts, level: level_byte(rms(&frame)), epoch: 0 };
        self.seq = self.seq.wrapping_add(1);
        match self.out.try_send(header.packet(&self.pkt[..n])) {
            Ok(()) => {
                self.sent.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

/// A running share-audio capture. Stops (and joins its thread) on [`ShareAudio::stop`] or
/// drop; both block for at most about one poll interval plus the source's close.
pub struct ShareAudio {
    stop: Arc<AtomicBool>,
    sent: Arc<AtomicU64>,
    join: Option<JoinHandle<()>>,
    source: ShareAudioSource,
}

impl std::fmt::Debug for ShareAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShareAudio").field("source", &self.source).field("sent", &self.packets_sent()).finish()
    }
}

fn open_source(source: ShareAudioSource) -> Result<Box<dyn StereoSource>> {
    match source {
        ShareAudioSource::TestTone => Ok(Box::new(ToneSource::new())),
        #[cfg(windows)]
        ShareAudioSource::ProcessTree(pid) => Ok(Box::new(crate::loopback::Loopback::open(pid, true)?)),
        #[cfg(windows)]
        ShareAudioSource::AllExcept(pid) => Ok(Box::new(crate::loopback::Loopback::open(pid, false)?)),
        #[cfg(not(windows))]
        _ => Err(AudioError::Unsupported("process loopback needs Windows".into())),
    }
}

impl ShareAudio {
    /// Opens `source` and starts sending datagrams to `out` (`try_send`: dropped when full).
    /// Blocks until the source is open (process loopback activation takes a few ms, at most
    /// a few seconds), so call it from a blocking context. Fails if the source cannot be
    /// opened; process loopback needs Windows 10 version 2004 or newer.
    pub fn start(source: ShareAudioSource, out: mpsc::Sender<Bytes>) -> Result<ShareAudio> {
        let stop = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicU64::new(0));
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
        let (thread_stop, thread_sent) = (stop.clone(), sent.clone());
        let join = std::thread::Builder::new()
            .name("tincan-share-audio".into())
            .spawn(move || {
                // The source lives on this thread only (COM objects of the loopback client).
                let src = open_source(source);
                let packetizer = Packetizer::new(&LibOpus, out, thread_sent);
                let (mut src, mut pk) = match (src, packetizer) {
                    (Ok(s), Ok(p)) => {
                        let _ = ready_tx.send(Ok(()));
                        (s, p)
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                run(&mut *src, &mut pk, &thread_stop);
            })
            .map_err(|e| AudioError::Thread(e.to_string()))?;
        let mut handle = ShareAudio { stop, sent, join: Some(join), source };
        match ready_rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(Ok(())) => Ok(handle),
            Ok(Err(e)) => {
                handle.join_thread();
                Err(e)
            }
            Err(_) => {
                // Still opening: let it go when it does.
                handle.stop.store(true, Ordering::Release);
                handle.join.take();
                Err(AudioError::Unsupported("the audio capture did not open in time".into()))
            }
        }
    }

    /// What this share captures.
    pub fn source(&self) -> ShareAudioSource {
        self.source
    }

    /// Datagrams handed to the channel so far.
    pub fn packets_sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// True while the capture thread runs (false after a capture error or once `out` closed).
    pub fn is_running(&self) -> bool {
        self.join.as_ref().is_some_and(|j| !j.is_finished())
    }

    /// Stops and joins the capture thread.
    pub fn stop(mut self) {
        self.join_thread();
    }

    fn join_thread(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take()
            && j.join().is_err()
        {
            tracing::error!("share audio thread panicked");
        }
    }
}

impl Drop for ShareAudio {
    fn drop(&mut self) {
        self.join_thread();
    }
}

fn run(src: &mut dyn StereoSource, pk: &mut Packetizer, stop: &AtomicBool) {
    let start = Instant::now();
    let mut buf = Vec::with_capacity(48 * 2 * 50);
    while !stop.load(Ordering::Acquire) {
        buf.clear();
        if let Err(e) = src.read(&mut buf) {
            tracing::warn!(%e, "share audio capture failed; the share continues without audio");
            return;
        }
        if !pk.push(&buf) {
            return;
        }
        let due = start.elapsed().as_micros() as u64 * 48 / 1000;
        if !pk.fill_to(due) {
            return;
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &mut mpsc::Receiver<Bytes>) -> Vec<Bytes> {
        let mut v = Vec::new();
        while let Ok(p) = rx.try_recv() {
            v.push(p);
        }
        v
    }

    #[test]
    fn packetizer_sends_contiguous_stereo_frames_without_speaking() {
        let (tx, mut rx) = mpsc::channel(1000);
        let sent = Arc::new(AtomicU64::new(0));
        let mut pk = Packetizer::new(&LibOpus, tx, sent.clone()).unwrap();
        let mut tone = ToneSource { start: Instant::now(), produced: 0 };
        // 1 s of tone in uneven pieces
        let mut buf = Vec::new();
        let mut total = 0;
        while total < 48_000 {
            let n = (total % 7 + 1) * 97;
            for _ in 0..n.min(48_000 - total) {
                let t = tone.produced as f32 / 48_000.0;
                buf.push(0.2 * (2.0 * std::f32::consts::PI * 220.0 * t).sin());
                buf.push(0.0);
                tone.produced += 1;
                total += 1;
            }
            assert!(pk.push(&buf));
            buf.clear();
        }
        let pkts = drain(&mut rx);
        assert_eq!(pkts.len(), 50, "1 s = 50 packets");
        assert_eq!(sent.load(Ordering::Relaxed), 50);
        let hs: Vec<AudioHeader> = pkts.iter().map(|p| AudioHeader::parse(p).unwrap().0).collect();
        assert!(hs.windows(2).all(|w| w[1].seq == w[0].seq.wrapping_add(1) && w[1].ts == w[0].ts.wrapping_add(960)));
        assert!(hs[0].has(AF_TALKSPURT) && hs.iter().skip(1).all(|h| h.flags == 0), "never AF_SPEAKING");
        assert!(pkts.iter().all(|p| p.len() <= 12 + SHARE_MAX_PAYLOAD));
        let mut dec = LibOpus.stereo_decoder().unwrap();
        let mut out = vec![0f32; 1920];
        let mut e = 0.0;
        for p in &pkts[5..] {
            dec.decode(&p[12..], &mut out).unwrap();
            e += out.iter().step_by(2).map(|x| x * x).sum::<f32>();
        }
        assert!(e > 10.0, "tone energy {e}");
    }

    #[test]
    fn silence_fills_a_quiet_source_and_the_backlog_is_bounded() {
        let (tx, mut rx) = mpsc::channel(1000);
        let mut pk = Packetizer::new(&LibOpus, tx, Arc::new(AtomicU64::new(0))).unwrap();
        // Nothing captured for 1 s of wall clock: silence keeps 50 packets a second going.
        for step in 1..=100u64 {
            assert!(pk.fill_to(step * 480));
        }
        let n = drain(&mut rx).len();
        assert!((46..=50).contains(&n), "{n} packets for 1 s of silence");
        // A source that dumps 2 s at once is cut to the backlog bound.
        let lots = vec![0.1f32; 96_000 * 2];
        assert!(pk.push(&lots));
        let n = drain(&mut rx).len();
        assert!(n <= (MAX_BACKLOG_MS as usize / 20) + 2, "{n} packets from a 2 s dump");
        // The receiver went away: the packetizer says stop.
        drop(rx);
        assert!(!pk.push(&[0.0; 1920 * 2]));
    }

    #[test]
    fn test_tone_share_runs_and_stops() {
        let (tx, mut rx) = mpsc::channel(1000);
        let share = ShareAudio::start(ShareAudioSource::TestTone, tx).unwrap();
        assert!(share.is_running());
        std::thread::sleep(Duration::from_millis(400));
        let n = share.packets_sent();
        assert!((10..=25).contains(&n), "{n} packets in 400 ms");
        share.stop();
        let got = drain(&mut rx).len() as u64;
        assert!(got >= n);
    }
}
