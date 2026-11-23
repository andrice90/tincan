//! Two engines with custom IO connected by an in-memory network with loss, jitter and
//! reordering. Runs in real time (about 8 s).

use bytes::Bytes;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tincan_audio::{AudioConfig, AudioEngine, AudioEvent, AudioIo, AudioSink, AudioSource, NullSink, SilenceSource};
use tincan_core::media::AudioHeader;

const BURST_MS: usize = 400;
const GAP_MS: usize = 800;
const PERIOD: usize = (BURST_MS + GAP_MS) * 48;

/// Speech-like bursts: pitch-modulated harmonics with a syllable envelope, then silence.
/// Records the wall-clock time at which each burst starts being read.
struct BurstSource {
    pos: usize,
    onsets: Arc<Mutex<Vec<Instant>>>,
}

fn speech(i: usize) -> f32 {
    let t = i as f32 / 48_000.0;
    let tau = 2.0 * std::f32::consts::PI;
    let phase = tau * (150.0 * t - 25.0 / (tau * 2.5) * (tau * 2.5 * t).cos());
    let env = 0.6 + 0.4 * (tau * 4.0 * t).sin();
    0.08 * env * (phase.sin() + 0.5 * (2.0 * phase).sin() + 0.25 * (3.0 * phase).sin())
}

impl AudioSource for BurstSource {
    fn read(&mut self, frame: &mut [f32]) {
        for s in frame.iter_mut() {
            let in_period = self.pos % PERIOD;
            if in_period == 0 {
                self.onsets.lock().unwrap().push(Instant::now());
            }
            *s = if in_period < BURST_MS * 48 { speech(self.pos) } else { 0.0 };
            self.pos += 1;
        }
    }
}

/// Records (time, rms) of every 10 ms block written to the speaker.
struct RecordingSink {
    blocks: Arc<Mutex<Vec<(Instant, f32)>>>,
}

impl AudioSink for RecordingSink {
    fn write(&mut self, frame: &[f32]) {
        let rms = (frame.iter().map(|x| x * x).sum::<f32>() / frame.len() as f32).sqrt();
        self.blocks.lock().unwrap().push((Instant::now(), rms));
    }
}

#[derive(Default, Debug)]
struct NetStats {
    sent: u64,
    lost: u64,
    delivered: u64,
    reordered: u64,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speech_over_lossy_jittery_network() {
    let onsets = Arc::new(Mutex::new(Vec::new()));
    let blocks = Arc::new(Mutex::new(Vec::new()));

    let (a, _a_events) = AudioEngine::new(AudioConfig::default()).unwrap();
    let (b, mut b_events) = AudioEngine::new(AudioConfig::default()).unwrap();
    a.set_io(AudioIo::Custom { source: Box::new(BurstSource { pos: 0, onsets: onsets.clone() }), sink: Box::new(NullSink) });
    b.set_io(AudioIo::Custom { source: Box::new(SilenceSource), sink: Box::new(RecordingSink { blocks: blocks.clone() }) });
    a.set_loss_hint(5);

    let (a_tx, mut a_rx) = tokio::sync::mpsc::channel::<Bytes>(256);
    let (b_tx, _b_rx) = tokio::sync::mpsc::channel::<Bytes>(256);
    b.start_call(8, 1, b_tx).unwrap();
    a.start_call(4, 1, a_tx).unwrap();

    // network: 5 % loss, 0..=60 ms uniform jitter (which reorders packets 20 ms apart)
    let stats = Arc::new(Mutex::new(NetStats::default()));
    let last_delivered = Arc::new(Mutex::new(None::<u16>));
    let net = {
        let (b, stats, last_delivered) = (b.clone(), stats.clone(), last_delivered.clone());
        tokio::spawn(async move {
            let mut rng = StdRng::seed_from_u64(0x7111_ca11);
            while let Some(pkt) = a_rx.recv().await {
                stats.lock().unwrap().sent += 1;
                if rng.random_bool(0.05) {
                    stats.lock().unwrap().lost += 1;
                    continue;
                }
                let delay = Duration::from_millis(rng.random_range(0..=60));
                let (b, stats, last) = (b.clone(), stats.clone(), last_delivered.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let seq = AudioHeader::parse(&pkt).map(|(h, _)| h.seq).unwrap_or(0);
                    {
                        let mut st = stats.lock().unwrap();
                        st.delivered += 1;
                        let mut last = last.lock().unwrap();
                        if let Some(l) = *last {
                            if (seq.wrapping_sub(l) as i16) < 0 {
                                st.reordered += 1;
                            } else {
                                *last = Some(seq);
                            }
                        } else {
                            *last = Some(seq);
                        }
                    }
                    b.push_packet(pkt);
                });
            }
        })
    };

    tokio::time::sleep(Duration::from_millis(8_000)).await;
    a.stop_call();
    b.stop_call();
    net.abort();

    let st = std::mem::take(&mut *stats.lock().unwrap());
    let onsets = onsets.lock().unwrap().clone();
    let blocks = blocks.lock().unwrap().clone();
    let mut events = Vec::new();
    while let Ok(e) = b_events.try_recv() {
        events.push(e);
    }
    println!("network: {st:?}");
    assert!(st.sent > 100 && st.lost > 0 && st.reordered > 0, "{st:?}");

    // Onsets in B's output: a loud block after at least 100 ms of quiet.
    const LOUD: f32 = 0.01;
    let mut out_onsets = Vec::new();
    let mut quiet_run = 10;
    for (t, rms) in &blocks {
        if *rms > LOUD {
            if quiet_run >= 10 {
                out_onsets.push(*t);
            }
            quiet_run = 0;
        } else {
            quiet_run += 1;
        }
    }

    // Match each source burst (after a 1 s warm-up) to the first output onset after it.
    let t_start = onsets[0];
    let mut latencies = Vec::new();
    let mut worst_gap_blocks = 0;
    let mut burst_energy = 0.0f32;
    for on in onsets.iter().filter(|t| t.duration_since(t_start) >= Duration::from_millis(1000)) {
        if on.elapsed() < Duration::from_millis(1000) {
            continue; // burst still in flight at the end of the run
        }
        let Some(out) = out_onsets.iter().find(|o| **o >= *on && o.duration_since(*on) < Duration::from_millis(600)) else {
            panic!("burst at {:?} never reached the speaker", on.duration_since(t_start));
        };
        let lat = out.duration_since(*on);
        latencies.push(lat.as_millis() as u64);
        // inside the received burst (skip 40 ms at either edge): no long gaps
        let lo = *out + Duration::from_millis(40);
        let hi = *out + Duration::from_millis(BURST_MS as u64 - 40);
        let mut run = 0;
        for (t, rms) in blocks.iter().filter(|(t, _)| *t >= lo && *t <= hi) {
            let _ = t;
            burst_energy += rms * rms;
            if *rms < 0.003 {
                run += 1;
                worst_gap_blocks = worst_gap_blocks.max(run);
            } else {
                run = 0;
            }
        }
    }
    latencies.sort_unstable();
    println!("burst latencies (ms): {latencies:?}, worst gap {} ms", worst_gap_blocks * 10);
    assert!(latencies.len() >= 4, "only {} bursts measured", latencies.len());
    let median = latencies[latencies.len() / 2];
    assert!(median < 250, "median mouth-to-ear latency {median} ms");
    let within = latencies.iter().filter(|l| **l < 250).count();
    assert!(within * 10 >= latencies.len() * 8, "latencies {latencies:?}");
    assert!(burst_energy > 0.05, "received energy {burst_energy}");
    assert!(worst_gap_blocks <= 6, "a {} ms gap inside a burst", worst_gap_blocks * 10);

    // B saw A's track speak and stop between bursts (800 ms gaps > 250 ms hangover + 300 ms hold).
    let speaking: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            AudioEvent::Speaking { track: 4, speaking } => Some(*speaking),
            _ => None,
        })
        .collect();
    println!("speaking events: {speaking:?}");
    assert!(speaking.iter().filter(|s| **s).count() >= 4, "{speaking:?}");
    assert!(speaking.windows(2).all(|w| w[0] != w[1]), "changes only: {speaking:?}");
    assert_eq!(speaking.first(), Some(&true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_io_idles_without_call_and_mic_test_reports_levels() {
    let (e, mut events) = AudioEngine::new(AudioConfig::default()).unwrap();
    let reads = Arc::new(Mutex::new(0usize));
    struct Counting(Arc<Mutex<usize>>);
    impl AudioSource for Counting {
        fn read(&mut self, frame: &mut [f32]) {
            *self.0.lock().unwrap() += 1;
            for (i, s) in frame.iter_mut().enumerate() {
                *s = 0.3 * (i as f32 * 0.1).sin();
            }
        }
    }
    e.set_io(AudioIo::Custom { source: Box::new(Counting(reads.clone())), sink: Box::new(NullSink) });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*reads.lock().unwrap(), 0, "nothing reads the source while idle");

    e.mic_test(true);
    tokio::time::sleep(Duration::from_millis(600)).await;
    e.mic_test(false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after_off = *reads.lock().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*reads.lock().unwrap(), after_off, "the clock stops with the mic test");
    let mut levels = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let AudioEvent::MicLevel(l) = ev {
            levels.push(l);
        }
    }
    // ~20 per second over 0.6 s
    assert!((6..=16).contains(&levels.len()), "{} level events", levels.len());
    assert!(levels.iter().all(|l| *l > 0.5 && *l <= 1.0), "{levels:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn is_running_turns_false_when_the_engine_thread_dies() {
    let (e, _events) = AudioEngine::new(AudioConfig::default()).unwrap();
    assert!(e.is_running());
    /// A microphone that panics on the engine thread at its first read.
    struct Panicking;
    impl AudioSource for Panicking {
        fn read(&mut self, _frame: &mut [f32]) {
            panic!("test: synthetic microphone failure");
        }
    }
    e.set_io(AudioIo::Custom { source: Box::new(Panicking), sink: Box::new(NullSink) });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(e.is_running(), "idle: nothing read the source yet");
    e.mic_test(true);
    let deadline = Instant::now() + Duration::from_secs(5);
    while e.is_running() {
        assert!(Instant::now() < deadline, "the engine thread never stopped");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    assert!(e.start_call(4, 1, tx).is_err(), "a stopped engine refuses commands");
}
