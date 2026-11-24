//! Neural noise suppression: DPDFNet (Apache-2.0, <https://github.com/ceva-ip/DPDFNet>), the
//! 48 kHz `dpdfnet2_48khz_hr` streaming model, run on the CPU with tract (pure Rust).
//!
//! The model takes one spectrum per 10 ms (960-point FFT, 20 ms Vorbis window, 50 % overlap)
//! and carries its recurrent state in an explicit tensor. This module does the causal STFT and
//! overlap-add around it, like the reference `StreamEnhancer` (`package/src/dpdfnet/stream.py`).
//! Output lags input by [`DELAY`]: one hop for the STFT plus 40 ms inside the model (measured by
//! cross-correlating clean speech with the output; the reference implementation has the same lag).
//!
//! The model file is not in git: `scripts/fetch-dpdfnet.ps1` puts it in `models/`, and build.rs
//! embeds it when present. Without it [`available`] is false and calls keep WebRTC noise
//! suppression.

use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, ensure};
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use tract_onnx::prelude::*;

/// Samples per call to [`Denoiser::process`] (10 ms at 48 kHz).
pub const HOP: usize = 480;
/// Samples between a sound going in and coming out (50 ms).
pub const DELAY: usize = 5 * HOP;
const WIN: usize = 2 * HOP;
const BINS: usize = WIN / 2 + 1;

#[cfg(dpdfnet_model)]
static ONNX: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/models/dpdfnet2_48khz_hr.onnx"
));
#[cfg(not(dpdfnet_model))]
static ONNX: &[u8] = &[];

struct Model {
    plan: Arc<TypedRunnableModel>,
    init_state: Tensor,
}

static MODEL: OnceLock<Option<Arc<Model>>> = OnceLock::new();

/// True when this build embeds the model.
pub fn available() -> bool {
    !ONNX.is_empty()
}

/// Loads and optimizes the model (about 150 ms, once per process). [`Denoiser::new`] does this
/// on first use; call it earlier from a thread that may block.
pub fn preload() {
    let _ = model();
}

fn model() -> Option<Arc<Model>> {
    MODEL
        .get_or_init(|| {
            if ONNX.is_empty() {
                return None;
            }
            let t = std::time::Instant::now();
            match load(ONNX) {
                Ok(m) => {
                    tracing::info!(ms = t.elapsed().as_millis() as u64, "dpdfnet model loaded");
                    Some(Arc::new(m))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "dpdfnet model failed to load; using WebRTC noise suppression");
                    None
                }
            }
        })
        .clone()
}

fn load(bytes: &[u8]) -> TractResult<Model> {
    let onnx = tract_onnx::onnx();
    let proto = onnx.proto_model_for_read(&mut &*bytes)?;
    let meta = |k: &str| {
        proto
            .metadata_props
            .iter()
            .find(|p| p.key == k)
            .map(|p| p.value.as_str())
            .ok_or_else(|| anyhow!("model metadata {k} missing"))
    };
    let num = |k: &str| -> TractResult<usize> { Ok(meta(k)?.trim().parse()?) };
    let list = |k: &str| -> TractResult<Vec<f32>> {
        meta(k)?
            .split(',')
            .map(|v| Ok(v.trim().parse::<f32>()?))
            .collect()
    };
    ensure!(
        num("sample_rate")? == 48_000
            && num("n_fft")? == WIN
            && num("hop_length")? == HOP
            && num("freq_bins")? == BINS,
        "unexpected model framing"
    );
    let erb = list("erb_norm_init")?;
    let spec = list("spec_norm_init")?;
    let size = num("state_size")?;
    ensure!(
        erb.len() == num("erb_norm_state_size")?
            && spec.len() == num("spec_norm_state_size")?
            && erb.len() + spec.len() <= size
    );
    let mut init = vec![0f32; size];
    init[..erb.len()].copy_from_slice(&erb);
    init[erb.len()..erb.len() + spec.len()].copy_from_slice(&spec);
    let plan = onnx
        .model_for_proto_model(&proto)?
        .into_optimized()?
        .into_runnable()?;
    Ok(Model {
        plan,
        init_state: tensor1(&init),
    })
}

/// The Vorbis window (DPDFNet's analysis and synthesis window; w[n]^2 + w[n + HOP]^2 = 1).
fn vorbis_window() -> Vec<f32> {
    let half = WIN as f64 / 2.0;
    (0..WIN)
        .map(|n| {
            let s = (0.5 * std::f64::consts::PI * (n as f64 + 0.5) / half).sin();
            (0.5 * std::f64::consts::PI * s * s).sin() as f32
        })
        .collect()
}

/// One stream's denoiser state. Feed it consecutive 10 ms frames of 48 kHz mono.
pub struct Denoiser {
    model: Arc<Model>,
    /// The model's recurrent state, fed back each frame.
    state: Option<Tensor>,
    window: Vec<f32>,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
    /// The last 20 ms of input.
    input: Vec<f32>,
    time: Vec<f32>,
    spec: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    spec_ri: Vec<f32>,
    /// Overlap-add accumulator; its first HOP samples are final after each frame.
    ola: Vec<f32>,
    failed: bool,
}

impl Denoiser {
    /// A fresh stream, or None when the model is not available.
    pub fn new() -> Option<Self> {
        let model = model()?;
        let mut planner = RealFftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(WIN);
        let inv = planner.plan_fft_inverse(WIN);
        let scratch_len = fwd.get_scratch_len().max(inv.get_scratch_len());
        Some(Self {
            state: Some(model.init_state.clone()),
            model,
            window: vorbis_window(),
            fwd,
            inv,
            input: vec![0.0; WIN],
            time: vec![0.0; WIN],
            spec: vec![Complex::default(); BINS],
            scratch: vec![Complex::default(); scratch_len],
            spec_ri: vec![0.0; BINS * 2],
            ola: vec![0.0; WIN],
            failed: false,
        })
    }

    /// Denoises 10 ms in place; the result is the input of [`DELAY`] earlier, cleaned.
    /// Frames of another length are left alone.
    pub fn process(&mut self, frame: &mut [f32]) {
        if frame.len() != HOP {
            return;
        }
        if let Err(e) = self.step(frame) {
            if !self.failed {
                tracing::warn!(error = %e, "dpdfnet frame failed");
                self.failed = true;
            }
            self.state = Some(self.model.init_state.clone());
        }
    }

    fn step(&mut self, frame: &mut [f32]) -> TractResult<()> {
        self.input.copy_within(HOP.., 0);
        self.input[HOP..].copy_from_slice(frame);
        for ((t, x), w) in self.time.iter_mut().zip(&self.input).zip(&self.window) {
            *t = x * w;
        }
        self.fwd
            .process_with_scratch(&mut self.time, &mut self.spec, &mut self.scratch)
            .map_err(|e| anyhow!("fft: {e}"))?;
        for (ri, c) in self.spec_ri.as_chunks_mut::<2>().0.iter_mut().zip(&self.spec) {
            ri[0] = c.re;
            ri[1] = c.im;
        }
        let spec = Tensor::from_shape(&[1, 1, BINS, 2], &self.spec_ri)?;
        let state = self
            .state
            .take()
            .unwrap_or_else(|| self.model.init_state.clone());
        // A fresh tract state per frame: a reused one keeps the GRUs' hidden state as op state
        // and feeds it back on top of `state_in`, so the output drifts from the reference.
        let mut out = self.model.plan.run(tvec!(spec.into(), state.into()))?;
        ensure!(out.len() == 2, "model returned {} outputs", out.len());
        self.state = Some(out.pop().expect("two outputs").into_tensor());
        let spec_e = out.pop().expect("two outputs");
        let view = spec_e.try_as_plain_ram()?;
        let spec_e = view.as_slice::<f32>()?;
        ensure!(
            spec_e.len() == BINS * 2,
            "model returned {} spectrum values",
            spec_e.len()
        );
        for (c, ri) in self.spec.iter_mut().zip(spec_e.as_chunks::<2>().0) {
            *c = Complex::new(ri[0], ri[1]);
        }
        // a real signal: no imaginary part at DC and Nyquist
        self.spec[0].im = 0.0;
        self.spec[BINS - 1].im = 0.0;
        self.inv
            .process_with_scratch(&mut self.spec, &mut self.time, &mut self.scratch)
            .map_err(|e| anyhow!("ifft: {e}"))?;
        // realfft's inverse is unnormalized
        let norm = 1.0 / WIN as f32;
        for ((o, t), w) in self.ola.iter_mut().zip(&self.time).zip(&self.window) {
            *o += t * w * norm;
        }
        frame.copy_from_slice(&self.ola[..HOP]);
        self.ola.copy_within(HOP.., 0);
        self.ola[HOP..].fill(0.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_is_power_complementary() {
        let w = vorbis_window();
        for n in 0..HOP {
            assert!((w[n] * w[n] + w[n + HOP] * w[n + HOP] - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    #[cfg_attr(not(dpdfnet_model), ignore = "needs scripts/fetch-dpdfnet.ps1")]
    fn removes_white_noise_and_keeps_its_length() {
        let mut d = Denoiser::new().expect("model");
        let mut seed = 1u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        };
        let (mut e_in, mut e_out) = (0.0f64, 0.0f64);
        for i in 0..300 {
            let mut f: Vec<f32> = (0..HOP).map(|_| noise() * 0.05).collect();
            let before: f64 = f.iter().map(|s| f64::from(s * s)).sum();
            d.process(&mut f);
            assert_eq!(f.len(), HOP);
            assert!(f.iter().all(|s| s.is_finite()));
            if i >= 100 {
                e_in += before;
                e_out += f.iter().map(|s| f64::from(s * s)).sum::<f64>();
            }
        }
        let db = 10.0 * (e_out / e_in).log10();
        assert!(db < -15.0, "white noise only down {db:.1} dB");
    }
}
