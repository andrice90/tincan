// Throwaway experiment: does sonora (pure-Rust WebRTC AEC3) + unsafe-libopus build on
// Windows MSVC, how much echo does AEC3 remove on a synthetic echo path, and what do they cost per frame.
use sonora::config::{EchoCanceller, NoiseSuppression};
use sonora::{AudioProcessing, Config, StreamConfig};
use std::time::Instant;
use unsafe_libopus::{
    OPUS_APPLICATION_VOIP, OPUS_OK, OPUS_SET_BITRATE_REQUEST, OPUS_SET_DTX_REQUEST,
    OPUS_SET_INBAND_FEC_REQUEST, OPUS_SET_PACKET_LOSS_PERC_REQUEST, OPUS_SET_COMPLEXITY_REQUEST,
    opus_decode_float, opus_decoder_create, opus_encode_float, opus_encoder_create,
    opus_encoder_ctl_impl, varargs,
};

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
    }
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / x.len() as f64).sqrt()
}

fn main() {
    let sr = 48_000usize;
    let sc = StreamConfig::new(sr as u32, 1);
    let n = sc.num_frames(); // 10 ms = 480
    let config = Config {
        echo_canceller: Some(EchoCanceller::default()),
        noise_suppression: if std::env::var("NS").is_ok() { Some(NoiseSuppression::default()) } else { None },
        ..Default::default()
    };
    let mut apm = AudioProcessing::builder()
        .config(config)
        .capture_config(sc)
        .render_config(sc)
        .build();

    // Far-end "speech": amplitude-modulated, low-passed noise. Echo path: 60 ms delay,
    // 3-tap smear, -6 dB. Near-end is silent, so everything in the mic is echo.
    let seconds = 20;
    let frames = seconds * 100;
    let delay = sr * 60 / 1000;
    let mut rng = Lcg(42);
    let total = frames * n;
    let mut far = vec![0f32; total];
    let mut lp = 0f32;
    for i in 0..total {
        lp = 0.9 * lp + 0.1 * rng.next();
        let env = 0.5 + 0.5 * ((i as f32 / sr as f32) * 2.0 * std::f32::consts::PI * 3.0).sin();
        far[i] = lp * env * 2.0;
    }
    let mut mic = vec![0f32; total];
    let mut echo = vec![0f32; total];
    let mut near = vec![0f32; total];
    let mut lp2 = 0f32;
    let mut rng2 = Lcg(7);
    for i in 0..total {
        lp2 = 0.8 * lp2 + 0.2 * rng2.next();
        let env = (0.5 + 0.5 * ((i as f32 / sr as f32) * 2.0 * std::f32::consts::PI * 1.7).sin()).powi(2);
        if i >= total / 2 { near[i] = lp2 * env * 1.5; }
    }
    for i in delay + 2..total {
        echo[i] = 0.5 * (0.6 * far[i - delay] + 0.3 * far[i - delay - 1] + 0.1 * far[i - delay - 2]);
        mic[i] = echo[i] + near[i] + 0.001 * rng.next();
    }
    let mut outall = vec![0f32; total];

    let mut out = vec![0f32; n];
    let mut rout = vec![0f32; n];
    let mut in_rms_tail = Vec::new();
    let mut out_rms_tail = Vec::new();
    let t0 = Instant::now();
    for f in 0..frames {
        let r = &far[f * n..(f + 1) * n];
        let c = &mic[f * n..(f + 1) * n];
        apm.process_render_f32(&[r], &mut [&mut rout]).unwrap();
        apm.process_capture_f32(&[c], &mut [&mut out]).unwrap();
        outall[f * n..(f + 1) * n].copy_from_slice(&out);
        if f >= frames / 2 {
            in_rms_tail.push(rms(c));
            out_rms_tail.push(rms(&out));
        }
    }
    let el = t0.elapsed();
    let i_r: f64 = in_rms_tail.iter().sum::<f64>() / in_rms_tail.len() as f64;
    let o_r: f64 = out_rms_tail.iter().sum::<f64>() / out_rms_tail.len() as f64;
    println!(
        "AEC3+NS 48k mono: {} frames of 10ms in {:?} => {:.1} us/frame ({:.3}% of one core)",
        frames,
        el,
        el.as_secs_f64() * 1e6 / frames as f64,
        el.as_secs_f64() / seconds as f64 * 100.0
    );
    let _ = (i_r, o_r);
    let seg = |a: &[f32]| (a.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / a.len() as f64).max(1e-20);
    // echo-only, after 5 s convergence: seconds 5..10
    let (a, b) = (5 * sr, 10 * sr);
    println!("echo-only ERLE (s5-10, NS {}): {:.1} dB", if std::env::var("NS").is_ok() {"on"} else {"off"}, 10.0 * (seg(&echo[a..b]) / seg(&outall[a..b])).log10());
    // double talk: seconds 12..20. Residual = out - near (assumes AEC passes near-end ~unchanged)
    let (a, b) = (12 * sr, 20 * sr);
    let resid: Vec<f32> = (a..b).map(|i| outall[i] - near[i]).collect();
    println!("double-talk: echo/residual {:.1} dB, near-end retained {:.1} dB (0 = untouched)", 10.0 * (seg(&echo[a..b]) / seg(&resid)).log10(), 10.0 * (seg(&outall[a..b]) / seg(&near[a..b])).log10());

    // Opus: 48 kHz mono, 20 ms, 32 kbps VOIP, in-band FEC + DTX, complexity 9.
    unsafe {
        let mut err = 0i32;
        let enc = opus_encoder_create(48_000, 1, OPUS_APPLICATION_VOIP, &mut err);
        assert!(err == OPUS_OK && !enc.is_null());
        for (req, v) in [
            (OPUS_SET_BITRATE_REQUEST, 32_000),
            (OPUS_SET_INBAND_FEC_REQUEST, 1),
            (OPUS_SET_PACKET_LOSS_PERC_REQUEST, 10),
            (OPUS_SET_DTX_REQUEST, 1),
            (OPUS_SET_COMPLEXITY_REQUEST, 9),
        ] {
            assert_eq!(opus_encoder_ctl_impl(enc, req, varargs![v]), OPUS_OK);
        }
        let dec = opus_decoder_create(48_000, 1, &mut err);
        assert!(err == OPUS_OK && !dec.is_null());
        let fs = 960usize;
        let mut pkt = vec![0u8; 4000];
        let mut pcm = vec![0f32; fs];
        let mut bytes = 0usize;
        let nfr = total / fs;
        let t = Instant::now();
        for k in 0..nfr {
            let len = opus_encode_float(enc, far[k * fs..].as_ptr(), fs as i32, pkt.as_mut_ptr(), 4000);
            assert!(len > 0);
            bytes += len as usize;
        }
        let te = t.elapsed();
        // decode incl. 10% packet loss handled by PLC (null data) or FEC (decode_fec=1 on next packet).
        let t = Instant::now();
        for k in 0..nfr {
            let len = opus_encode_float(enc, far[k * fs..].as_ptr(), fs as i32, pkt.as_mut_ptr(), 4000);
            if k % 10 == 5 {
                // lost: run PLC
                opus_decode_float(dec, std::ptr::null(), 0, pcm.as_mut_ptr(), fs as i32, 0);
            } else {
                opus_decode_float(dec, pkt.as_ptr(), len, pcm.as_mut_ptr(), fs as i32, 0);
            }
        }
        let td = t.elapsed();
        println!(
            "Opus enc 20ms/32kbps/FEC/DTX c9: {:.1} us/frame, avg {:.1} kbps (incl. DTX on silence gaps)",
            te.as_secs_f64() * 1e6 / nfr as f64,
            bytes as f64 * 8.0 / seconds as f64 / 1000.0
        );
        println!(
            "Opus enc+dec (10% PLC) : {:.1} us/frame",
            td.as_secs_f64() * 1e6 / nfr as f64
        );
    }
}
