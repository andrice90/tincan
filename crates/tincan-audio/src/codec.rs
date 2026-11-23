//! Opus codec abstraction.
//!
//! The engine only talks to the [`OpusCodec`], [`OpusEncode`] and [`OpusDecode`] traits, so the
//! unmaintained `unsafe-libopus` (libopus 1.3.1 transpiled to Rust) can be swapped for another
//! Opus implementation later. Everything is fixed at 48 kHz mono, which is all Tincan uses.

use crate::error::{AudioError, Result};
use unsafe_libopus::{
    OPUS_ALLOC_FAIL, OPUS_APPLICATION_VOIP, OPUS_BAD_ARG, OPUS_BUFFER_TOO_SMALL,
    OPUS_GET_IN_DTX_REQUEST, OPUS_INTERNAL_ERROR, OPUS_INVALID_PACKET, OPUS_INVALID_STATE,
    OPUS_OK, OPUS_RESET_STATE, OPUS_SET_BITRATE_REQUEST, OPUS_SET_COMPLEXITY_REQUEST,
    OPUS_SET_DTX_REQUEST, OPUS_SET_INBAND_FEC_REQUEST, OPUS_SET_PACKET_LOSS_PERC_REQUEST,
    OPUS_SET_SIGNAL_REQUEST, OPUS_SET_VBR_REQUEST, OPUS_SIGNAL_VOICE, OPUS_UNIMPLEMENTED,
    OpusDecoder, OpusEncoder, opus_decode_float, opus_decoder_create, opus_decoder_ctl_impl,
    opus_decoder_destroy, opus_encode_float, opus_encoder_create, opus_encoder_ctl_impl,
    opus_encoder_destroy, varargs,
};

/// Sample rate of every Opus stream in Tincan.
pub const OPUS_RATE: u32 = 48_000;
/// Largest Opus packet (one frame) per RFC 6716.
pub const MAX_OPUS_PACKET: usize = 1275;

/// Settings applied when an encoder is created. Defaults are the Tincan voice settings from
/// `docs/architecture.md`: 32 kbps VBR, complexity 7, in-band FEC on, 10 % expected loss, DTX on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderSettings {
    /// Target bitrate in bits per second.
    pub bitrate_bps: i32,
    /// Encoder complexity 0..=10.
    pub complexity: i32,
    /// In-band forward error correction (LBRR) on or off.
    pub fec: bool,
    /// Expected packet loss in percent (drives how much FEC is spent).
    pub packet_loss_perc: i32,
    /// Discontinuous transmission on or off.
    pub dtx: bool,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self { bitrate_bps: 32_000, complexity: 7, fec: true, packet_loss_perc: 10, dtx: true }
    }
}

/// A 48 kHz mono Opus encoder (VOIP application, voice signal).
pub trait OpusEncode: Send {
    /// Encodes one frame (`pcm.len()` must be a valid Opus frame size at 48 kHz, Tincan uses
    /// 960 = 20 ms) into `out` and returns the packet length. A length of 1 or 2 bytes is a DTX
    /// frame that should not be transmitted.
    fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize>;
    /// Sets the target bitrate in bits per second.
    fn set_bitrate(&mut self, bps: i32) -> Result<()>;
    /// Sets the expected packet loss in percent (0..=100).
    fn set_packet_loss_perc(&mut self, pct: i32) -> Result<()>;
    /// Turns discontinuous transmission on or off.
    fn set_dtx(&mut self, on: bool) -> Result<()>;
    /// True if the encoder is currently in DTX (emitting silence frames).
    fn in_dtx(&mut self) -> Result<bool>;
}

/// A 48 kHz mono Opus decoder.
pub trait OpusDecode: Send {
    /// Decodes `packet` into `out` and returns the number of samples written.
    fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize>;
    /// Reconstructs the frame *before* `next_packet` from its in-band FEC data (falls back to
    /// packet loss concealment when the packet carries none). `out.len()` must equal the
    /// duration of the lost frame.
    fn decode_fec(&mut self, next_packet: &[u8], out: &mut [f32]) -> Result<usize>;
    /// Packet loss concealment: synthesizes `out.len()` samples continuing the last frame.
    fn conceal(&mut self, out: &mut [f32]) -> Result<usize>;
    /// Resets the decoder state (new stream).
    fn reset(&mut self);
}

/// Factory for Opus encoders and decoders. Implement this to swap the Opus implementation.
pub trait OpusCodec: Send + Sync {
    /// Creates a 48 kHz mono VOIP encoder with `settings` applied.
    fn encoder(&self, settings: &EncoderSettings) -> Result<Box<dyn OpusEncode>>;
    /// Creates a 48 kHz mono decoder.
    fn decoder(&self) -> Result<Box<dyn OpusDecode>>;
}

/// [`OpusCodec`] backed by `unsafe-libopus` (pure Rust libopus 1.3.1).
#[derive(Clone, Copy, Debug, Default)]
pub struct LibOpus;

impl OpusCodec for LibOpus {
    fn encoder(&self, settings: &EncoderSettings) -> Result<Box<dyn OpusEncode>> {
        Ok(Box::new(LibOpusEncoder::new(settings)?))
    }

    fn decoder(&self) -> Result<Box<dyn OpusDecode>> {
        Ok(Box::new(LibOpusDecoder::new()?))
    }
}

fn opus_error(code: i32) -> AudioError {
    let name = match code {
        OPUS_BAD_ARG => "bad argument",
        OPUS_BUFFER_TOO_SMALL => "buffer too small",
        OPUS_INTERNAL_ERROR => "internal error",
        OPUS_INVALID_PACKET => "invalid packet",
        OPUS_UNIMPLEMENTED => "unimplemented",
        OPUS_INVALID_STATE => "invalid state",
        OPUS_ALLOC_FAIL => "allocation failed",
        _ => "unknown error",
    };
    AudioError::Codec { code, name }
}

fn check(code: i32) -> Result<()> {
    if code == OPUS_OK { Ok(()) } else { Err(opus_error(code)) }
}

/// Valid Opus frame sizes at 48 kHz (2.5 to 60 ms).
fn valid_frame(n: usize) -> bool {
    matches!(n, 120 | 240 | 480 | 960 | 1920 | 2880)
}

fn len_i32(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// Encoder state owned exclusively by this value.
struct LibOpusEncoder {
    st: std::ptr::NonNull<OpusEncoder>,
}

// SAFETY: the encoder state is a plain heap block owned exclusively by this value and only
// touched through `&mut self`; libopus keeps no thread-local or global state.
unsafe impl Send for LibOpusEncoder {}

impl LibOpusEncoder {
    fn new(s: &EncoderSettings) -> Result<Self> {
        let mut err = 0i32;
        // SAFETY: valid rate/channels/application constants and a valid out pointer.
        let raw = unsafe { opus_encoder_create(OPUS_RATE as i32, 1, OPUS_APPLICATION_VOIP, &mut err) };
        check(err)?;
        let st = std::ptr::NonNull::new(raw).ok_or(opus_error(OPUS_ALLOC_FAIL))?;
        let mut enc = Self { st };
        enc.ctl(OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_VOICE)?;
        enc.ctl(OPUS_SET_VBR_REQUEST, 1)?;
        enc.ctl(OPUS_SET_BITRATE_REQUEST, s.bitrate_bps)?;
        enc.ctl(OPUS_SET_COMPLEXITY_REQUEST, s.complexity.clamp(0, 10))?;
        enc.ctl(OPUS_SET_INBAND_FEC_REQUEST, i32::from(s.fec))?;
        enc.ctl(OPUS_SET_PACKET_LOSS_PERC_REQUEST, s.packet_loss_perc.clamp(0, 100))?;
        enc.ctl(OPUS_SET_DTX_REQUEST, i32::from(s.dtx))?;
        Ok(enc)
    }

    fn ctl(&mut self, request: i32, value: i32) -> Result<()> {
        // SAFETY: `st` is a live encoder; every request used here takes one i32 argument.
        check(unsafe { opus_encoder_ctl_impl(self.st.as_ptr(), request, varargs![value]) })
    }
}

impl Drop for LibOpusEncoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_encoder_create and destroyed exactly once.
        unsafe { opus_encoder_destroy(self.st.as_ptr()) }
    }
}

impl OpusEncode for LibOpusEncoder {
    fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize> {
        if !valid_frame(pcm.len()) || out.is_empty() {
            return Err(opus_error(OPUS_BAD_ARG));
        }
        // SAFETY: pointers and lengths come from live slices; libopus reads `pcm.len()`
        // samples and writes at most `out.len()` bytes.
        let n = unsafe {
            opus_encode_float(
                self.st.as_ptr(),
                pcm.as_ptr(),
                len_i32(pcm.len()),
                out.as_mut_ptr(),
                len_i32(out.len()),
            )
        };
        if n < 0 { Err(opus_error(n)) } else { Ok(n as usize) }
    }

    fn set_bitrate(&mut self, bps: i32) -> Result<()> {
        self.ctl(OPUS_SET_BITRATE_REQUEST, bps)
    }

    fn set_packet_loss_perc(&mut self, pct: i32) -> Result<()> {
        self.ctl(OPUS_SET_PACKET_LOSS_PERC_REQUEST, pct.clamp(0, 100))
    }

    fn set_dtx(&mut self, on: bool) -> Result<()> {
        self.ctl(OPUS_SET_DTX_REQUEST, i32::from(on))
    }

    fn in_dtx(&mut self) -> Result<bool> {
        let mut v = 0i32;
        // SAFETY: OPUS_GET_IN_DTX_REQUEST takes one `&mut i32` out argument.
        check(unsafe { opus_encoder_ctl_impl(self.st.as_ptr(), OPUS_GET_IN_DTX_REQUEST, varargs![&mut v]) })?;
        Ok(v != 0)
    }
}

/// Decoder state owned exclusively by this value.
struct LibOpusDecoder {
    st: std::ptr::NonNull<OpusDecoder>,
}

// SAFETY: see `LibOpusEncoder`.
unsafe impl Send for LibOpusDecoder {}

impl LibOpusDecoder {
    fn new() -> Result<Self> {
        let mut err = 0i32;
        // SAFETY: valid rate/channels and a valid out pointer.
        let raw = unsafe { opus_decoder_create(OPUS_RATE as i32, 1, &mut err) };
        check(err)?;
        let st = std::ptr::NonNull::new(raw).ok_or(opus_error(OPUS_ALLOC_FAIL))?;
        Ok(Self { st })
    }

    fn run(&mut self, data: Option<&[u8]>, out: &mut [f32], fec: bool) -> Result<usize> {
        if !valid_frame(out.len()) {
            return Err(opus_error(OPUS_BAD_ARG));
        }
        let (ptr, len) = match data {
            Some(d) if !d.is_empty() && d.len() <= MAX_OPUS_PACKET => (d.as_ptr(), len_i32(d.len())),
            Some(_) => return Err(opus_error(OPUS_INVALID_PACKET)),
            None => (std::ptr::null(), 0),
        };
        // SAFETY: `ptr`/`len` describe a live slice (or null/0 for PLC, which libopus accepts);
        // libopus writes at most `out.len()` samples.
        let n = unsafe {
            opus_decode_float(self.st.as_ptr(), ptr, len, out.as_mut_ptr(), len_i32(out.len()), i32::from(fec))
        };
        if n < 0 { Err(opus_error(n)) } else { Ok(n as usize) }
    }
}

impl Drop for LibOpusDecoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_decoder_create and destroyed exactly once.
        unsafe { opus_decoder_destroy(self.st.as_ptr()) }
    }
}

impl OpusDecode for LibOpusDecoder {
    fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize> {
        self.run(Some(packet), out, false)
    }

    fn decode_fec(&mut self, next_packet: &[u8], out: &mut [f32]) -> Result<usize> {
        self.run(Some(next_packet), out, true)
    }

    fn conceal(&mut self, out: &mut [f32]) -> Result<usize> {
        self.run(None, out, false)
    }

    fn reset(&mut self) {
        // SAFETY: OPUS_RESET_STATE takes no arguments.
        let rc = unsafe { opus_decoder_ctl_impl(self.st.as_ptr(), OPUS_RESET_STATE, varargs![]) };
        if rc != OPUS_OK {
            tracing::debug!(rc, "opus decoder reset failed");
        }
    }
}

/// True if an Opus packet's TOC byte says it carries SILK data (SILK-only or hybrid mode), the
/// only modes with in-band FEC.
pub fn packet_has_silk(packet: &[u8]) -> bool {
    packet.first().is_some_and(|toc| (toc >> 3) < 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, offset: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = (i + offset) as f32 / 48_000.0;
                0.3 * (2.0 * std::f32::consts::PI * 220.0 * t).sin()
                    + 0.15 * (2.0 * std::f32::consts::PI * 660.0 * t).sin()
            })
            .collect()
    }

    #[test]
    fn encode_decode_plc_fec_roundtrip() {
        let codec = LibOpus;
        let mut enc = codec.encoder(&EncoderSettings::default()).unwrap();
        let mut dec = codec.decoder().unwrap();
        let mut pkt = [0u8; MAX_OPUS_PACKET];
        let mut pcm = vec![0f32; 960];
        let mut packets = Vec::new();
        for k in 0..20 {
            let n = enc.encode(&tone(960, k * 960), &mut pkt).unwrap();
            assert!(n > 2, "voice frame should not be DTX");
            packets.push(pkt[..n].to_vec());
        }
        let mut energy = 0.0f32;
        for p in &packets[..10] {
            assert_eq!(dec.decode(p, &mut pcm).unwrap(), 960);
            energy += pcm.iter().map(|x| x * x).sum::<f32>();
        }
        assert!(energy > 1.0, "decoded audio has energy");
        // Frame 10 lost: recover from frame 11's FEC (or PLC fallback), then decode 11.
        assert_eq!(dec.decode_fec(&packets[11], &mut pcm).unwrap(), 960);
        assert_eq!(dec.decode(&packets[11], &mut pcm).unwrap(), 960);
        assert_eq!(dec.conceal(&mut pcm).unwrap(), 960);
        dec.reset();
        assert_eq!(dec.decode(&packets[12], &mut pcm).unwrap(), 960);
        assert!(packet_has_silk(&packets[12]) || !packets[12].is_empty());
    }

    #[test]
    fn dtx_on_digital_silence() {
        let mut enc = LibOpus.encoder(&EncoderSettings::default()).unwrap();
        let mut pkt = [0u8; MAX_OPUS_PACKET];
        let silence = vec![0f32; 960];
        let mut sent = 0;
        let mut saw_dtx = false;
        for _ in 0..150 {
            let n = enc.encode(&silence, &mut pkt).unwrap();
            if n > 2 {
                sent += 1;
            } else {
                saw_dtx = true;
                assert!(enc.in_dtx().unwrap());
            }
        }
        assert!(saw_dtx);
        assert!(sent < 30, "3 s of silence sent {sent} packets");
        enc.set_dtx(false).unwrap();
        let n = enc.encode(&silence, &mut pkt).unwrap();
        assert!(n > 2, "DTX off sends every frame");
    }

    #[test]
    fn rejects_bad_sizes() {
        let mut enc = LibOpus.encoder(&EncoderSettings::default()).unwrap();
        let mut pkt = [0u8; 100];
        assert!(enc.encode(&[0.0; 100], &mut pkt).is_err());
        let mut dec = LibOpus.decoder().unwrap();
        let mut pcm = [0f32; 960];
        assert!(dec.decode(&[], &mut pcm).is_err());
        assert!(dec.decode(&[0u8; 2000], &mut pcm).is_err());
    }
}
