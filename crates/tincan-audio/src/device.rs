//! cpal (WASAPI) device enumeration and streams.
//!
//! Callbacks do only allocation-free, lock-free work: convert samples, downmix/upmix, move them
//! through a [`SampleRing`], store a latency figure in an atomic and unpark the engine thread.
//! Stream errors are forwarded as their [`cpal::ErrorKind`] over a bounded channel.

use crate::error::{AudioError, Result};
use crate::resample::StreamResampler;
use crate::ring::SampleRing;
use crate::{AudioDevices, DeviceInfo};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use crossbeam_channel::Sender;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::Thread;
use std::time::Duration;

/// Internal processing rate.
pub(crate) const ENGINE_RATE: u32 = 48_000;
/// How long cpal may take to initialize a stream.
const OPEN_TIMEOUT: Duration = Duration::from_secs(3);

/// Direction of a device stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dir {
    Input,
    Output,
}

impl Dir {
    pub(crate) fn noun(self) -> &'static str {
        match self {
            Dir::Input => "microphone",
            Dir::Output => "speaker",
        }
    }
}

/// A stream error reported by a cpal error callback.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fault {
    pub generation: u64,
    pub dir: Dir,
    pub kind: cpal::ErrorKind,
}

fn dev_err(e: impl std::fmt::Display) -> AudioError {
    AudioError::Device(e.to_string())
}

/// An open capture stream. Dropping it stops the stream.
pub(crate) struct InputDevice {
    _stream: cpal::Stream,
    pub generation: u64,
    pub name: String,
    pub rate: u32,
    /// Mono samples at `rate`.
    pub ring: Arc<SampleRing>,
    /// Capture-to-callback latency in microseconds.
    pub latency_us: Arc<AtomicU32>,
    /// Device rate -> 48 kHz, if the device is not at 48 kHz.
    pub resampler: Option<StreamResampler>,
    /// Scratch for one resampler input chunk.
    pub scratch: Vec<f32>,
}

/// An open playback stream. Dropping it stops the stream.
pub(crate) struct OutputDevice {
    _stream: cpal::Stream,
    pub generation: u64,
    pub name: String,
    pub rate: u32,
    /// Mono samples at `rate`.
    pub ring: Arc<SampleRing>,
    /// Callback-to-playback latency in microseconds.
    pub latency_us: Arc<AtomicU32>,
    /// Frames the last callback asked for.
    pub cb_frames: Arc<AtomicU32>,
    /// 48 kHz -> device rate, if the device is not at 48 kHz.
    pub resampler: Option<StreamResampler>,
    /// 48 kHz samples waiting for a full resampler chunk.
    pub pending: Vec<f32>,
}

/// Lists input and output devices. Runs on a short-lived thread so cpal's COM initialization
/// never touches the caller's thread (which may be a UI STA or a tokio worker).
pub(crate) fn enumerate() -> Result<AudioDevices> {
    std::thread::Builder::new()
        .name("tincan-audio-enum".into())
        .spawn(enumerate_here)
        .map_err(|e| AudioError::Thread(e.to_string()))?
        .join()
        .map_err(|_| AudioError::Device("device enumeration panicked".into()))?
}

fn enumerate_here() -> Result<AudioDevices> {
    let host = cpal::default_host();
    let id_of = |d: Option<cpal::Device>| d.and_then(|d| d.id().ok()).map(|i| i.to_string());
    let default_in = id_of(host.default_input_device());
    let default_out = id_of(host.default_output_device());
    let info = |d: cpal::Device, default: Option<&str>| -> Option<DeviceInfo> {
        let id = d.id().ok()?.to_string();
        let name = d.description().map(|x| x.name().to_string()).unwrap_or_else(|_| d.to_string());
        Some(DeviceInfo { is_default: default == Some(id.as_str()), id, name })
    };
    let inputs = host.input_devices().map_err(dev_err)?.filter_map(|d| info(d, default_in.as_deref())).collect();
    let outputs = host.output_devices().map_err(dev_err)?.filter_map(|d| info(d, default_out.as_deref())).collect();
    Ok(AudioDevices { inputs, outputs })
}

/// Finds the selected device, or the system default (which cpal follows) when `selected` is
/// `None`. Returns the device and whether a selected device was missing (default used instead).
fn pick(host: &cpal::Host, selected: Option<&str>, dir: Dir) -> Result<(cpal::Device, bool)> {
    let mut missing = false;
    if let Some(id) = selected {
        if let Some(d) = cpal::DeviceId::from_str(id).ok().and_then(|did| host.device_by_id(&did)) {
            return Ok((d, false));
        }
        missing = true;
    }
    let d = match dir {
        Dir::Input => host.default_input_device(),
        Dir::Output => host.default_output_device(),
    };
    d.map(|d| (d, missing)).ok_or(AudioError::NoDevice(dir.noun()))
}

fn device_name(d: &cpal::Device) -> String {
    d.description().map(|x| x.name().to_string()).unwrap_or_else(|_| d.to_string())
}

struct Callbacks {
    ring: Arc<SampleRing>,
    latency_us: Arc<AtomicU32>,
    cb_frames: Arc<AtomicU32>,
    waker: Thread,
    faults: Sender<Fault>,
    generation: u64,
    dir: Dir,
}

impl Callbacks {
    fn error_fn(&self) -> impl FnMut(cpal::Error) + Send + 'static {
        let faults = self.faults.clone();
        let waker = self.waker.clone();
        let (generation, dir) = (self.generation, self.dir);
        move |e: cpal::Error| {
            let _ = faults.try_send(Fault { generation, dir, kind: e.kind() });
            waker.unpark();
        }
    }
}

fn micros(d: Duration) -> u32 {
    u32::try_from(d.as_micros()).unwrap_or(u32::MAX)
}

fn build_input<T>(device: &cpal::Device, config: cpal::StreamConfig, cb: &Callbacks) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let ch = usize::from(config.channels.max(1));
    let inv = 1.0 / ch as f32;
    let ring = cb.ring.clone();
    let latency = cb.latency_us.clone();
    let waker = cb.waker.clone();
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                ring.push_iter(data.chunks_exact(ch).map(|f| f.iter().map(|s| f32::from_sample(*s)).sum::<f32>() * inv));
                let ts = info.timestamp();
                latency.store(micros(ts.callback.saturating_duration_since(ts.capture)), Ordering::Relaxed);
                waker.unpark();
            },
            cb.error_fn(),
            Some(OPEN_TIMEOUT),
        )
        .map_err(dev_err)
}

fn build_output<T>(device: &cpal::Device, config: cpal::StreamConfig, cb: &Callbacks) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let ch = usize::from(config.channels.max(1));
    let ring = cb.ring.clone();
    let latency = cb.latency_us.clone();
    let frames_hint = cb.cb_frames.clone();
    let waker = cb.waker.clone();
    device
        .build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                let frames = data.len() / ch;
                frames_hint.store(u32::try_from(frames).unwrap_or(u32::MAX), Ordering::Relaxed);
                let mut chunk = [0f32; 256];
                for block in data.chunks_mut(chunk.len() * ch) {
                    let n = block.len() / ch;
                    let got = ring.pop_slice(&mut chunk[..n]);
                    chunk[got..n].fill(0.0);
                    for (frame, s) in block.chunks_exact_mut(ch).zip(&chunk[..n]) {
                        let v = T::from_sample(*s);
                        frame.fill(v);
                    }
                }
                let ts = info.timestamp();
                latency.store(micros(ts.playback.saturating_duration_since(ts.callback)), Ordering::Relaxed);
                waker.unpark();
            },
            cb.error_fn(),
            Some(OPEN_TIMEOUT),
        )
        .map_err(dev_err)
}

fn unsupported(fmt: SampleFormat) -> AudioError {
    AudioError::Device(format!("unsupported sample format {fmt}"))
}

fn input_for_format(fmt: SampleFormat, d: &cpal::Device, c: cpal::StreamConfig, cb: &Callbacks) -> Result<cpal::Stream> {
    match fmt {
        SampleFormat::F32 => build_input::<f32>(d, c, cb),
        SampleFormat::F64 => build_input::<f64>(d, c, cb),
        SampleFormat::I8 => build_input::<i8>(d, c, cb),
        SampleFormat::I16 => build_input::<i16>(d, c, cb),
        SampleFormat::I24 => build_input::<cpal::I24>(d, c, cb),
        SampleFormat::I32 => build_input::<i32>(d, c, cb),
        SampleFormat::U8 => build_input::<u8>(d, c, cb),
        SampleFormat::U16 => build_input::<u16>(d, c, cb),
        SampleFormat::U32 => build_input::<u32>(d, c, cb),
        other => Err(unsupported(other)),
    }
}

fn output_for_format(fmt: SampleFormat, d: &cpal::Device, c: cpal::StreamConfig, cb: &Callbacks) -> Result<cpal::Stream> {
    match fmt {
        SampleFormat::F32 => build_output::<f32>(d, c, cb),
        SampleFormat::F64 => build_output::<f64>(d, c, cb),
        SampleFormat::I8 => build_output::<i8>(d, c, cb),
        SampleFormat::I16 => build_output::<i16>(d, c, cb),
        SampleFormat::I24 => build_output::<cpal::I24>(d, c, cb),
        SampleFormat::I32 => build_output::<i32>(d, c, cb),
        SampleFormat::U8 => build_output::<u8>(d, c, cb),
        SampleFormat::U16 => build_output::<u16>(d, c, cb),
        SampleFormat::U32 => build_output::<u32>(d, c, cb),
        other => Err(unsupported(other)),
    }
}

/// Opens the capture stream (native format; WASAPI shared capture does not convert).
/// Returns the device and whether the selected device was missing.
pub(crate) fn open_input(selected: Option<&str>, waker: Thread, faults: Sender<Fault>, generation: u64) -> Result<(InputDevice, bool)> {
    let host = cpal::default_host();
    let (device, missing) = pick(&host, selected, Dir::Input)?;
    let def = device.default_input_config().map_err(dev_err)?;
    let rate = def.sample_rate();
    let cb = Callbacks {
        ring: Arc::new(SampleRing::new(rate as usize / 2)),
        latency_us: Arc::new(AtomicU32::new(0)),
        cb_frames: Arc::new(AtomicU32::new(0)),
        waker,
        faults,
        generation,
        dir: Dir::Input,
    };
    let config = cpal::StreamConfig { channels: def.channels(), sample_rate: rate, buffer_size: cpal::BufferSize::Default };
    let stream = input_for_format(def.sample_format(), &device, config, &cb)?;
    stream.play().map_err(dev_err)?;
    let resampler = if rate == ENGINE_RATE { None } else { Some(StreamResampler::new(rate, ENGINE_RATE, rate as usize / 100)?) };
    let chunk = resampler.as_ref().map_or(480, StreamResampler::input_frames_next);
    let name = device_name(&device);
    tracing::info!(%name, rate, channels = def.channels(), format = %def.sample_format(), "microphone opened");
    Ok((
        InputDevice {
            _stream: stream,
            generation,
            name,
            rate,
            ring: cb.ring,
            latency_us: cb.latency_us,
            resampler,
            scratch: vec![0.0; chunk.max(480) * 2],
        },
        missing,
    ))
}

/// Opens the playback stream, at 48 kHz if Windows accepts it (WASAPI shared render converts),
/// else at the device's native rate with rubato resampling.
pub(crate) fn open_output(selected: Option<&str>, waker: Thread, faults: Sender<Fault>, generation: u64) -> Result<(OutputDevice, bool)> {
    let host = cpal::default_host();
    let (device, missing) = pick(&host, selected, Dir::Output)?;
    let def = device.default_output_config().map_err(dev_err)?;
    let make_cb = |rate: u32| Callbacks {
        ring: Arc::new(SampleRing::new(rate as usize / 5)),
        latency_us: Arc::new(AtomicU32::new(0)),
        cb_frames: Arc::new(AtomicU32::new(0)),
        waker: waker.clone(),
        faults: faults.clone(),
        generation,
        dir: Dir::Output,
    };
    let config = |rate: u32| cpal::StreamConfig { channels: def.channels(), sample_rate: rate, buffer_size: cpal::BufferSize::Default };
    let mut rate = ENGINE_RATE;
    let mut cb = make_cb(rate);
    let stream = match output_for_format(def.sample_format(), &device, config(rate), &cb) {
        Ok(s) => s,
        Err(e) if def.sample_rate() != ENGINE_RATE => {
            tracing::debug!(%e, native = def.sample_rate(), "48 kHz output refused, using the native rate");
            rate = def.sample_rate();
            cb = make_cb(rate);
            output_for_format(def.sample_format(), &device, config(rate), &cb)?
        }
        Err(e) => return Err(e),
    };
    stream.play().map_err(dev_err)?;
    let resampler = if rate == ENGINE_RATE { None } else { Some(StreamResampler::new(ENGINE_RATE, rate, 480)?) };
    let name = device_name(&device);
    tracing::info!(%name, rate, channels = def.channels(), format = %def.sample_format(), "speaker opened");
    Ok((
        OutputDevice {
            _stream: stream,
            generation,
            name,
            rate,
            ring: cb.ring,
            latency_us: cb.latency_us,
            cb_frames: cb.cb_frames,
            resampler,
            pending: Vec::with_capacity(4096),
        },
        missing,
    ))
}
