//! Clip decoding (WAV via hound, FLAC and MP3 built by hand) and soundboard playback through the
//! public engine API with custom IO.

use bytes::Bytes;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;
use tincan_audio::{AudioConfig, AudioEngine, AudioError, AudioEvent, AudioIo, LibOpus, NullSink, OpusCodec, SilenceSource};
use tincan_core::media::{AF_SOUNDBOARD, AudioHeader};

fn sine(rate: u32, hz: f32, secs: f32, amp: f32) -> Vec<f32> {
    let n = (rate as f32 * secs) as usize;
    (0..n).map(|i| amp * (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin()).collect()
}

/// 16-bit PCM WAV with `channels` copies of `mono` (the right channel inverted in stereo).
fn wav(rate: u32, channels: u16, mono: &[f32]) -> Vec<u8> {
    let spec = hound::WavSpec { channels, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut cur = Cursor::new(Vec::new());
    {
        let mut w = hound::WavWriter::new(&mut cur, spec).unwrap();
        for s in mono {
            let v = (s * 32767.0) as i16;
            w.write_sample(v).unwrap();
            if channels == 2 {
                w.write_sample(v / 2).unwrap();
            }
        }
        w.finalize().unwrap();
    }
    cur.into_inner()
}

fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
    }
    crc
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for b in data {
        crc ^= u16::from(*b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
        }
    }
    crc
}

/// Minimal FLAC encoder: 16-bit mono, VERBATIM subframes, fixed 4096-sample blocks.
fn flac(rate: u32, mono: &[f32]) -> Vec<u8> {
    const BLOCK: usize = 4096;
    let total = mono.len() as u64;
    let mut out = b"fLaC".to_vec();
    out.push(0x80); // last metadata block, STREAMINFO
    out.extend_from_slice(&[0, 0, 34]);
    out.extend_from_slice(&(BLOCK as u16).to_be_bytes()); // min block
    out.extend_from_slice(&(BLOCK as u16).to_be_bytes()); // max block
    out.extend_from_slice(&[0; 6]); // min/max frame size unknown
    // 20 bits rate | 3 bits channels-1 (0: mono) | 5 bits bps-1 | 36 bits total samples
    let packed: u64 = (u64::from(rate) << 44) | (15u64 << 36) | total;
    out.extend_from_slice(&packed.to_be_bytes());
    out.extend_from_slice(&[0; 16]); // MD5 unknown
    for (n, chunk) in mono.chunks(BLOCK).enumerate() {
        let mut f = vec![0xFF, 0xF8]; // sync, fixed blocking
        // block size: 0111 = 16-bit (size-1) at the end of the header; rate 0000 = STREAMINFO
        f.push(0b0111_0000);
        // channels 0000 = mono, sample size 100 = 16 bit
        f.push(0b0000_1000);
        assert!(n < 128);
        f.push(n as u8); // UTF-8 frame number (one byte)
        f.extend_from_slice(&((chunk.len() - 1) as u16).to_be_bytes());
        f.push(crc8(&f));
        f.push(0b0000_0010); // VERBATIM subframe, no wasted bits
        for s in chunk {
            f.extend_from_slice(&((s * 32767.0) as i16).to_be_bytes());
        }
        let c = crc16(&f);
        f.extend_from_slice(&c.to_be_bytes());
        out.extend_from_slice(&f);
    }
    out
}

/// MPEG-1 Layer III, 44.1 kHz, 128 kbps, mono, `frames` silent frames (zero side info and
/// main data decode to digital silence). 1152 samples per frame.
fn silent_mp3(frames: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..frames {
        let mut f = vec![0u8; 417];
        f[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0xC0]);
        out.extend_from_slice(&f);
    }
    out
}

#[test]
fn wav_44k1_stereo_resamples_to_48k_mono_with_correct_duration() {
    let mono = sine(44_100, 440.0, 1.5, 0.5);
    let clip = AudioEngine::decode_clip(&wav(44_100, 2, &mono), 20_000).unwrap();
    let expected = 72_000i64; // 1.5 s at 48 kHz
    assert!((clip.len() as i64 - expected).abs() <= 2, "{} samples", clip.len());
    assert!((1499..=1501).contains(&clip.duration_ms()), "{} ms", clip.duration_ms());
    // stereo (L, L/2) averages to 0.75 L
    let mid = &clip.samples()[24_000..48_000];
    let rms = (mid.iter().map(|x| x * x).sum::<f32>() / mid.len() as f32).sqrt();
    assert!((rms - 0.75 * 0.5 / 2f32.sqrt()).abs() < 0.02, "rms {rms}");
}

#[test]
fn max_ms_is_enforced() {
    let bytes = wav(48_000, 1, &sine(48_000, 300.0, 3.0, 0.3));
    match AudioEngine::decode_clip(&bytes, 2_000) {
        Err(AudioError::ClipTooLong { max_ms: 2_000, duration_ms }) => assert!(duration_ms > 2_000),
        other => panic!("expected ClipTooLong, got {other:?}"),
    }
    assert_eq!(AudioEngine::decode_clip(&bytes, 3_000).unwrap().len(), 144_000);
}

#[test]
fn flac_decodes() {
    let mono = sine(44_100, 660.0, 1.0, 0.4);
    let clip = AudioEngine::decode_clip(&flac(44_100, &mono), 20_000).unwrap();
    assert!((clip.len() as i64 - 48_000).abs() <= 2, "{} samples", clip.len());
    let mid = &clip.samples()[12_000..36_000];
    let rms = (mid.iter().map(|x| x * x).sum::<f32>() / mid.len() as f32).sqrt();
    assert!((rms - 0.4 / 2f32.sqrt()).abs() < 0.02, "rms {rms}");
}

#[test]
fn mp3_decodes() {
    let clip = AudioEngine::decode_clip(&silent_mp3(40), 20_000).unwrap();
    // 40 * 1152 samples at 44.1 kHz = 1045 ms (decoders may trim the codec delay)
    let ms = clip.duration_ms();
    assert!((980..=1050).contains(&ms), "{ms} ms");
    assert!(clip.samples().iter().all(|s| s.abs() < 1e-4));
    assert!(matches!(AudioEngine::decode_clip(&silent_mp3(40), 500), Err(AudioError::ClipTooLong { .. })));
}

#[test]
fn garbage_is_rejected() {
    assert!(matches!(AudioEngine::decode_clip(b"definitely not audio", 20_000), Err(AudioError::ClipDecode(_))));
    assert!(AudioEngine::decode_clip(&[], 20_000).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn soundboard_while_muted_is_sent_flagged_with_energy() {
    let clip = Arc::new(AudioEngine::decode_clip(&wav(44_100, 2, &sine(44_100, 440.0, 1.0, 0.5)), 20_000).unwrap());
    let (e, mut events) = AudioEngine::new(AudioConfig::default()).unwrap();
    e.set_io(AudioIo::Custom { source: Box::new(SilenceSource), sink: Box::new(NullSink) });
    assert!(matches!(e.play_clip(clip.clone()), Err(AudioError::NotInCall)));

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(512);
    e.start_call(12, 3, tx).unwrap();
    e.set_muted(true);
    e.play_clip(clip.clone()).unwrap();
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    let mut dec = LibOpus.decoder().unwrap();
    let mut pcm = vec![0f32; 960];
    let (mut flagged, mut energy) = (0, 0.0f32);
    while let Ok(p) = rx.try_recv() {
        let (h, opus) = AudioHeader::parse(&p).unwrap();
        assert_eq!((h.track, h.epoch), (12, 3));
        if h.flags & AF_SOUNDBOARD != 0 {
            flagged += 1;
            dec.decode(opus, &mut pcm).unwrap();
            energy += pcm.iter().map(|x| x * x).sum::<f32>();
        }
    }
    assert!((45..=52).contains(&flagged), "{flagged} soundboard packets for a 1 s clip");
    assert!(energy / (flagged as f32 * 960.0) > 0.01, "energy {energy}");
    let mut finished = false;
    while let Ok(ev) = events.try_recv() {
        finished |= ev == AudioEvent::ClipFinished;
    }
    assert!(finished, "ClipFinished emitted");

    e.set_deafened(true);
    assert!(matches!(e.play_clip(clip.clone()), Err(AudioError::Deafened)));
    e.set_deafened(false);
    e.stop_call();
    assert!(matches!(e.play_clip(clip), Err(AudioError::NotInCall)));
}
