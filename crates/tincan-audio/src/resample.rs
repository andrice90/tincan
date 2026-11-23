//! Mono resampling with rubato: a streaming wrapper for device rates and a whole-clip helper.

use crate::error::{AudioError, Result};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

fn err(e: impl std::fmt::Display) -> AudioError {
    AudioError::Resample(e.to_string())
}

/// Streaming mono resampler with a fixed input chunk. Buffers are allocated once at
/// construction; `process` does not allocate.
pub(crate) struct StreamResampler {
    rs: Fft<f32>,
    out: Vec<f32>,
}

impl StreamResampler {
    /// Resampler from `from` Hz to `to` Hz taking chunks of about `chunk` input frames.
    pub(crate) fn new(from: u32, to: u32, chunk: usize) -> Result<Self> {
        let rs = Fft::<f32>::new(from as usize, to as usize, chunk.max(1), 1, FixedSync::Input).map_err(err)?;
        let out = vec![0f32; rs.output_frames_max()];
        Ok(Self { rs, out })
    }

    /// Exact number of input frames the next `process` call needs.
    pub(crate) fn input_frames_next(&self) -> usize {
        self.rs.input_frames_next()
    }

    /// Resamples exactly `input_frames_next()` frames and returns the produced output.
    pub(crate) fn process(&mut self, input: &[f32]) -> Result<&[f32]> {
        let need = self.rs.input_frames_next();
        if input.len() != need {
            return Err(AudioError::Resample(format!("expected {need} frames, got {}", input.len())));
        }
        let inp = InterleavedSlice::new(input, 1, need).map_err(err)?;
        let cap = self.out.len();
        let mut outp = InterleavedSlice::new_mut(&mut self.out[..], 1, cap).map_err(err)?;
        let (_, produced) = self.rs.process_into_buffer(&inp, &mut outp, None).map_err(err)?;
        Ok(&self.out[..produced])
    }
}

/// Resamples a whole mono clip, trimming the resampler delay so the output lines up with the
/// input and has `len * to / from` frames.
pub(crate) fn resample_clip(samples: &[f32], from: u32, to: u32) -> Result<Vec<f32>> {
    if from == to || samples.is_empty() {
        return Ok(samples.to_vec());
    }
    let mut rs = Fft::<f32>::new(from as usize, to as usize, 1024, 1, FixedSync::Input).map_err(err)?;
    let inp = InterleavedSlice::new(samples, 1, samples.len()).map_err(err)?;
    let out = rs.process_all(&inp, samples.len(), None).map_err(err)?;
    Ok(out.take_data())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5).collect()
    }

    #[test]
    fn streaming_44k1_to_48k_keeps_rate_and_energy() {
        let mut rs = StreamResampler::new(44_100, 48_000, 441).unwrap();
        let input = sine(44_100, 440.0, 44_100);
        let mut pos = 0;
        let mut out = Vec::new();
        loop {
            let n = rs.input_frames_next();
            if pos + n > input.len() {
                break;
            }
            out.extend_from_slice(rs.process(&input[pos..pos + n]).unwrap());
            pos += n;
        }
        let expected = pos as f64 * 48_000.0 / 44_100.0;
        assert!((out.len() as f64 - expected).abs() < 2.0, "{} vs {expected}", out.len());
        let tail = &out[out.len() / 2..];
        let rms = (tail.iter().map(|x| x * x).sum::<f32>() / tail.len() as f32).sqrt();
        assert!((rms - 0.3535).abs() < 0.02, "rms {rms}");
    }

    #[test]
    fn clip_resample_length() {
        // rubato rounds the expected length up, so allow one frame
        let out = resample_clip(&sine(44_100, 440.0, 22_050), 44_100, 48_000).unwrap();
        assert!((out.len() as i64 - 24_000).abs() <= 1, "{}", out.len());
        let out = resample_clip(&sine(16_000, 440.0, 1_600), 16_000, 48_000).unwrap();
        assert!((out.len() as i64 - 4_800).abs() <= 1, "{}", out.len());
    }
}
