//! Small allocation-free DSP helpers: levels, limiter, fades and speaking detection.

/// Post-APM frame level above which the local user counts as speaking (dBov).
pub const SPEAKING_THRESHOLD_DBOV: f32 = -45.0;
/// Hangover after the last loud frame before the local speaking flag drops (ms).
pub const SPEAKING_HANGOVER_MS: u32 = 250;
/// Consecutive speaking packets a receiver needs before lighting the speaking ring.
pub const REMOTE_SPEAKING_PACKETS: u8 = 2;
/// How long a receiver keeps the ring lit after the last speaking packet (ms).
pub const REMOTE_SPEAKING_HOLD_MS: u64 = 300;

/// Root mean square of a frame (0 for an empty frame).
pub fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    let sum: f32 = x.iter().map(|v| v * v).sum();
    (sum / x.len() as f32).sqrt()
}

/// Level in dB relative to full scale (an RMS of 1.0 is 0 dBov). Returns -127 for silence.
pub fn dbov(rms: f32) -> f32 {
    if rms <= 1e-7 { -127.0 } else { (20.0 * rms.log10()).max(-127.0) }
}

/// RFC 6464 style level byte: 0..=127 = -dBov (0 loudest, 127 silence).
pub fn level_byte(rms: f32) -> u8 {
    (-dbov(rms)).round().clamp(0.0, 127.0) as u8
}

/// Maps an RMS to the 0..1 meter value of the `mic-level` UI event (-60 dBFS .. 0 dBFS).
pub fn meter_level(rms: f32) -> f32 {
    ((dbov(rms) + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Knee of the soft limiter: samples below this magnitude pass unchanged.
const LIMIT_KNEE: f32 = 0.8;

/// Soft limiter: identity below the knee, smooth tanh compression above, never reaching 1.0.
#[inline]
pub fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= LIMIT_KNEE {
        x
    } else {
        let room = 1.0 - LIMIT_KNEE;
        let y = LIMIT_KNEE + room * ((a - LIMIT_KNEE) / room).tanh();
        y.copysign(x)
    }
}

/// Applies [`soft_limit`] to every sample.
pub fn soft_limit_all(buf: &mut [f32]) {
    for s in buf {
        *s = soft_limit(*s);
    }
}

/// Linear fade from 1 to 0 across the buffer.
pub fn fade_out(buf: &mut [f32]) {
    let n = buf.len().max(1) as f32;
    for (i, s) in buf.iter_mut().enumerate() {
        *s *= 1.0 - (i as f32 + 1.0) / n;
    }
}

/// Linear fade from 0 to 1 over the first `len` samples.
pub fn fade_in(buf: &mut [f32], len: usize) {
    let len = len.min(buf.len()).max(1);
    for (i, s) in buf.iter_mut().take(len).enumerate() {
        *s *= i as f32 / len as f32;
    }
}

/// Local voice activity from post-APM frame energy with a hangover.
#[derive(Debug, Default)]
pub struct LocalVad {
    hang_ms: u32,
}

impl LocalVad {
    /// Feeds one frame's level; returns whether the speaking flag is set for it.
    pub fn update(&mut self, level_dbov: f32, frame_ms: u32) -> bool {
        if level_dbov > SPEAKING_THRESHOLD_DBOV {
            self.hang_ms = SPEAKING_HANGOVER_MS;
            true
        } else if self.hang_ms > 0 {
            self.hang_ms = self.hang_ms.saturating_sub(frame_ms);
            true
        } else {
            false
        }
    }

    /// Clears the hangover (used on mute).
    pub fn reset(&mut self) {
        self.hang_ms = 0;
    }
}

/// Receiver-side speaking state of one remote track, from the header speaking flag.
#[derive(Debug, Default)]
pub struct RemoteSpeaking {
    consecutive: u8,
    speaking: bool,
    last_speaking_ms: u64,
}

impl RemoteSpeaking {
    /// Feeds one arriving packet. Returns `Some(new_state)` when the state changes.
    pub fn on_packet(&mut self, speaking_flag: bool, now_ms: u64) -> Option<bool> {
        if speaking_flag {
            self.consecutive = self.consecutive.saturating_add(1);
            self.last_speaking_ms = now_ms;
            if self.consecutive >= REMOTE_SPEAKING_PACKETS && !self.speaking {
                self.speaking = true;
                return Some(true);
            }
        } else {
            self.consecutive = 0;
        }
        None
    }

    /// Expires the hold. Returns `Some(false)` when the ring turns off.
    pub fn on_tick(&mut self, now_ms: u64) -> Option<bool> {
        if self.speaking && now_ms.saturating_sub(self.last_speaking_ms) > REMOTE_SPEAKING_HOLD_MS {
            self.speaking = false;
            self.consecutive = 0;
            return Some(false);
        }
        None
    }

    /// Current state.
    pub fn speaking(&self) -> bool {
        self.speaking
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(level_byte(0.0), 127);
        assert_eq!(level_byte(1.0), 0);
        assert_eq!(level_byte(0.1), 20);
        assert!((meter_level(1.0) - 1.0).abs() < 1e-6);
        assert_eq!(meter_level(0.0), 0.0);
    }

    #[test]
    fn limiter_is_bounded_and_monotonic() {
        let mut prev = -2.0f32;
        for i in 0..=400 {
            let x = -2.0 + i as f32 * 0.01;
            let y = soft_limit(x);
            assert!(y.abs() < 1.0);
            assert!(y >= prev - 1e-6);
            prev = y;
        }
        assert_eq!(soft_limit(0.5), 0.5);
    }

    #[test]
    fn local_vad_hangover() {
        let mut v = LocalVad::default();
        assert!(v.update(-20.0, 20));
        // 250 ms hangover = 13 quiet frames still flagged
        let held = (0..20).take_while(|_| v.update(-90.0, 20)).count();
        assert_eq!(held, 13);
        assert!(!v.update(-90.0, 20));
    }

    #[test]
    fn remote_speaking_needs_two_packets_and_holds() {
        let mut r = RemoteSpeaking::default();
        assert_eq!(r.on_packet(true, 0), None);
        assert_eq!(r.on_packet(false, 20), None);
        assert_eq!(r.on_packet(true, 40), None);
        assert_eq!(r.on_packet(true, 60), Some(true));
        assert_eq!(r.on_tick(300), None);
        assert_eq!(r.on_tick(361), Some(false));
        assert!(!r.speaking());
    }
}
