//! Wrapper around sonora's WebRTC audio processing (AEC3, noise suppression, AGC2, high-pass).

use sonora::config::{
    AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter, MaxProcessingRate, NoiseSuppression, Pipeline,
};
use sonora::{AudioProcessing, Config, StreamConfig};

/// Samples per APM frame (10 ms at 48 kHz).
pub(crate) const APM_FRAME: usize = 480;

/// One APM instance for one call. Frames are 10 ms of 48 kHz mono.
pub(crate) struct Apm {
    inner: AudioProcessing,
    scratch: Vec<f32>,
    render_out: Vec<f32>,
}

fn config(echo_cancellation: bool, noise_suppression: bool) -> Config {
    Config {
        // Process at 48 kHz: no internal 48->32->48 kHz resampling and full-band output.
        pipeline: Pipeline { maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz, ..Default::default() },
        high_pass_filter: Some(HighPassFilter::default()),
        echo_canceller: echo_cancellation.then(EchoCanceller::default),
        noise_suppression: noise_suppression.then(NoiseSuppression::default),
        // Adaptive digital AGC + limiter. The analog input volume controller stays off: we never
        // touch the OS microphone volume.
        gain_controller2: Some(GainController2 {
            input_volume_controller: false,
            adaptive_digital: Some(AdaptiveDigital::default()),
            fixed_digital: Default::default(),
        }),
        ..Default::default()
    }
}

impl Apm {
    /// Builds the processing chain.
    pub(crate) fn new(echo_cancellation: bool, noise_suppression: bool) -> Self {
        let sc = StreamConfig::new(48_000, 1);
        let inner = AudioProcessing::builder()
            .config(config(echo_cancellation, noise_suppression))
            .capture_config(sc)
            .render_config(sc)
            .build();
        Self { inner, scratch: vec![0.0; APM_FRAME], render_out: vec![0.0; APM_FRAME] }
    }

    /// Turns echo cancellation and noise suppression on or off (AGC2 and the high-pass filter
    /// stay on).
    pub(crate) fn set_processing(&mut self, echo_cancellation: bool, noise_suppression: bool) {
        self.inner.apply_config(config(echo_cancellation, noise_suppression));
    }

    /// Feeds the far-end reference: the exact 10 ms that go to the speaker.
    pub(crate) fn render(&mut self, frame: &[f32]) {
        if frame.len() != APM_FRAME {
            return;
        }
        if let Err(e) = self.inner.process_render_f32(&[frame], &mut [&mut self.render_out[..]]) {
            tracing::debug!(%e, "apm render");
        }
    }

    /// Processes 10 ms of microphone audio in place.
    pub(crate) fn capture(&mut self, frame: &mut [f32]) {
        if frame.len() != APM_FRAME {
            return;
        }
        self.scratch.copy_from_slice(frame);
        if let Err(e) = self.inner.process_capture_f32(&[&self.scratch[..]], &mut [frame]) {
            tracing::debug!(%e, "apm capture");
        }
    }

    /// Hint for the delay between a render frame and the capture frame that contains its echo.
    pub(crate) fn set_delay_ms(&mut self, ms: i32) {
        // Clamping to 0..=500 is reported as an error but processing continues.
        let _ = self.inner.set_stream_delay_ms(ms.clamp(0, 500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_speech_like_signal_and_silence() {
        let mut apm = Apm::new(true, true);
        let mut energy_in = 0f32;
        let mut energy_out = 0f32;
        let mut frame = vec![0f32; APM_FRAME];
        for f in 0..300 {
            for (i, s) in frame.iter_mut().enumerate() {
                let t = (f * APM_FRAME + i) as f32 / 48_000.0;
                let pitch = 140.0 + 30.0 * (2.0 * std::f32::consts::PI * 3.0 * t).sin();
                let env = 0.5 + 0.5 * (2.0 * std::f32::consts::PI * 4.0 * t).sin();
                *s = 0.05 * env * (2.0 * std::f32::consts::PI * pitch * t).sin();
            }
            energy_in += frame.iter().map(|x| x * x).sum::<f32>();
            apm.render(&[0.0; APM_FRAME]);
            apm.capture(&mut frame);
            if f >= 100 {
                energy_out += frame.iter().map(|x| x * x).sum::<f32>();
            }
        }
        assert!(energy_out > 0.1 * energy_in * (200.0 / 300.0), "in {energy_in} out {energy_out}");
        // digital silence stays (near) silent once filter tails have decayed
        let mut silent = vec![0f32; APM_FRAME];
        for _ in 0..20 {
            silent.fill(0.0);
            apm.render(&[0.0; APM_FRAME]);
            apm.capture(&mut silent);
        }
        let peak = silent.iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!(peak < 1e-3, "peak {peak}");
    }
}
