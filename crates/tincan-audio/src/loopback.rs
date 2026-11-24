//! Windows process loopback capture for screen share audio (Windows 10 2004 or newer).
//!
//! `ActivateAudioInterfaceAsync` on the virtual device `VAD\Process_Loopback` with
//! `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` gives an `IAudioClient` that captures the
//! render audio of one process tree (`INCLUDE_TARGET_PROCESS_TREE`, a shared window's app) or of
//! everything but one process tree (`EXCLUDE_TARGET_PROCESS_TREE`, a shared display minus
//! Tincan). There is no fallback to plain system loopback: that would capture Tincan's own
//! playback (the call) and send it back to the viewers.
//!
//! All COM objects live on the capture thread that opened them (`Loopback` is not `Send`).
//! Every `unsafe` block below is a Win32/COM call with arguments built right there; each has its
//! own SAFETY note.

use crate::error::{AudioError, Result};
use crate::share::StereoSource;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
use windows::Win32::System::Variant::VT_BLOB;
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
use windows::core::{HRESULT, IUnknown, Interface, implement};

/// `WAVE_FORMAT_IEEE_FLOAT`.
const FORMAT_FLOAT: u16 = 3;
/// Capture buffer (100 ns units): 200 ms.
const BUFFER_HNS: i64 = 2_000_000;
/// How long activation may take.
const ACTIVATE_TIMEOUT: Duration = Duration::from_secs(4);
/// Packets read per `read` at most (bounds the time one poll can take).
const MAX_PACKETS_PER_READ: usize = 64;

fn err(what: &str, e: windows::core::Error) -> AudioError {
    AudioError::Unsupported(format!("{what}: {e}"))
}

/// The process that owns a top-level window (`window:<HWND>` share sources). None if the
/// window is gone.
pub fn window_process_id(hwnd: isize) -> Option<u32> {
    let mut pid = 0u32;
    // SAFETY: GetWindowThreadProcessId only reads the handle value and writes the pid out
    // parameter, which points at a live local; an invalid handle returns 0.
    let tid = unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut core::ffi::c_void), Some(&mut pid)) };
    (tid != 0 && pid != 0).then_some(pid)
}

/// True if `pid` is this process or one of its ancestors (the terminal that started Tincan,
/// say). Capturing such a process tree would capture Tincan too. Errs on the safe side: an
/// unreadable process list counts as true.
pub fn is_self_or_ancestor(pid: u32) -> bool {
    let me = std::process::id();
    if pid == me {
        return true;
    }
    let Some(parents) = parent_map() else { return true };
    let mut cur = me;
    for _ in 0..64 {
        match parents.get(&cur) {
            Some(&p) if p == pid => return true,
            Some(&p) if p != 0 && p != cur => cur = p,
            _ => return false,
        }
    }
    false
}

/// pid -> parent pid of every running process.
fn parent_map() -> Option<std::collections::HashMap<u32, u32>> {
    // SAFETY: a process snapshot handle, closed below; no other arguments.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
    let mut map = std::collections::HashMap::new();
    let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    // SAFETY: `snap` is a live snapshot and `e` a correctly sized entry (dwSize set).
    let mut ok = unsafe { Process32FirstW(snap, &mut e) }.is_ok();
    while ok {
        map.insert(e.th32ProcessID, e.th32ParentProcessID);
        // SAFETY: as above.
        ok = unsafe { Process32NextW(snap, &mut e) }.is_ok();
    }
    // SAFETY: closes the snapshot handle created above, once.
    let _ = unsafe { CloseHandle(snap) };
    Some(map)
}

/// COM for the capture thread (MTA), released when dropped.
struct Com;

impl Com {
    fn init() -> Result<Com> {
        // SAFETY: initializes COM for the calling thread; balanced by CoUninitialize in Drop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok().map_err(|e| err("COM", e))?;
        Ok(Com)
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx of `init` on this thread.
        unsafe { CoUninitialize() }
    }
}

/// Signals activation completion. Agile, so Windows may call it from any thread.
#[implement(IActivateAudioInterfaceCompletionHandler, windows::Win32::System::Com::IAgileObject)]
struct Activated(std::sync::Mutex<Option<SyncSender<()>>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        if let Some(tx) = self.0.lock().ok().and_then(|mut g| g.take()) {
            let _ = tx.send(());
        }
        Ok(())
    }
}

impl windows::Win32::System::Com::IAgileObject_Impl for Activated_Impl {}

/// A running process loopback capture: 48 kHz stereo float (Windows converts).
pub(crate) struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    // Dropped last: the COM objects above go first.
    _com: Com,
}

impl Loopback {
    /// Opens a capture of `pid`'s process tree (`include`) or of everything but it.
    pub(crate) fn open(pid: u32, include: bool) -> Result<Loopback> {
        let com = Com::init()?;
        let mode = if include { PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE } else { PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE };
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId: pid, ProcessLoopbackMode: mode },
            },
        };
        // The windows crate gives PROPVARIANT a Drop that calls PropVariantClear, which would
        // CoTaskMemFree the blob below (it points at `params` on the stack: heap corruption).
        // The blob is borrowed, so the variant must never be cleared: ManuallyDrop.
        let mut pv = std::mem::ManuallyDrop::new(PROPVARIANT::default());
        // SAFETY: writes the VT_BLOB tag and a blob that points at `params`, which outlives
        // the activation call below (it copies the blob). `pv` is never dropped (see above).
        unsafe {
            let inner = &mut *pv.Anonymous.Anonymous;
            inner.vt = VT_BLOB;
            inner.Anonymous.blob = BLOB {
                cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                pBlobData: (&params as *const AUDIOCLIENT_ACTIVATION_PARAMS).cast_mut().cast(),
            };
        }
        let (tx, rx) = sync_channel(1);
        let handler: IActivateAudioInterfaceCompletionHandler = Activated(std::sync::Mutex::new(Some(tx))).into();
        // SAFETY: the device path is a static wide string, the IID that of IAudioClient, `pv`
        // a valid VT_BLOB PROPVARIANT and `handler` a live COM object.
        let op = unsafe { ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv), &handler) }
            .map_err(|e| err("activation", e))?;
        rx.recv_timeout(ACTIVATE_TIMEOUT).map_err(|_| AudioError::Unsupported("activation timed out".into()))?;
        let mut hr = HRESULT(0);
        let mut unk: Option<IUnknown> = None;
        // SAFETY: activation completed (the handler fired); both out pointers are live locals.
        unsafe { op.GetActivateResult(&mut hr, &mut unk) }.map_err(|e| err("activation result", e))?;
        hr.ok().map_err(|e| err("activation", e))?;
        let client: IAudioClient = unk.ok_or_else(|| AudioError::Unsupported("no audio client".into()))?.cast().map_err(|e| err("audio client", e))?;
        let fmt = WAVEFORMATEX {
            wFormatTag: FORMAT_FLOAT,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        // SAFETY: `fmt` is a complete WAVEFORMATEX that lives across the call. The process
        // loopback client has no mix format of its own, so the format is ours; AUTOCONVERTPCM
        // makes Windows convert to it.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_HNS,
                0,
                &fmt,
                None,
            )
        }
        .map_err(|e| err("initialize", e))?;
        // SAFETY: the client is initialized, so the capture service exists.
        let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(|e| err("capture service", e))?;
        // SAFETY: starts the initialized stream.
        unsafe { client.Start() }.map_err(|e| err("start", e))?;
        tracing::info!(pid, include, "share audio capture opened");
        Ok(Loopback { client, capture, _com: com })
    }
}

impl StereoSource for Loopback {
    fn read(&mut self, out: &mut Vec<f32>) -> Result<()> {
        for _ in 0..MAX_PACKETS_PER_READ {
            // SAFETY: plain call on the live capture client.
            let next = unsafe { self.capture.GetNextPacketSize() }.map_err(|e| err("capture", e))?;
            if next == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            // SAFETY: out pointers are live locals. On success `data` points at `frames`
            // frames of the format we initialized (2 x f32), valid until ReleaseBuffer.
            unsafe { self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }.map_err(|e| err("capture", e))?;
            let n = frames as usize * 2;
            if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                out.resize(out.len() + n, 0.0);
            } else {
                // SAFETY: see GetBuffer above; the slice is copied before ReleaseBuffer.
                let s = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), n) };
                out.extend(s.iter().map(|x| if x.is_finite() { x.clamp(-1.0, 1.0) } else { 0.0 }));
            }
            // SAFETY: releases exactly the frames GetBuffer returned.
            unsafe { self.capture.ReleaseBuffer(frames) }.map_err(|e| err("capture", e))?;
        }
        Ok(())
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        // SAFETY: stops the stream started in `open`; the client is still alive here.
        let _ = unsafe { self.client.Stop() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestry_and_window_lookup() {
        assert!(is_self_or_ancestor(std::process::id()));
        // The parent of this test process (cargo) is an ancestor.
        let parents = parent_map().expect("process list");
        if let Some(&p) = parents.get(&std::process::id()) {
            assert!(is_self_or_ancestor(p));
        }
        // A pid nobody has.
        assert!(!is_self_or_ancestor(u32::MAX - 3), "a pid that does not exist");
        assert_eq!(window_process_id(0), None);
    }
}
