//! Tincan screen share: source listing, capture and hardware H.264 encode.
//!
//! Pipeline (per share, on a dedicated `tincan-screen` thread):
//!
//! 1. **Capture** with Windows.Graphics.Capture through `moq_video::capture`.
//!    Frames arrive as NV12 textures on the GPU. WGC only produces new
//!    pictures when the screen changes; moq-video repeats the last picture at
//!    the capture rate (twice the mode's frame cap here, because its pacing
//!    sleep is rounded up to the 15.6 ms Windows timer tick). The loop is
//!    variable-frame-rate safe (pauses, minimized windows, reopens).
//! 2. **Rate limit** to the mode's frame cap over capture timestamps.
//! 3. **Downscale** on the GPU (D3D11 video processor) when the source is
//!    wider than 1920 or larger than the mode's pixel budget, keeping aspect
//!    and even dimensions. A repeated (unchanged) picture reuses its scaled
//!    copy.
//! 4. **Encode** with the Media Foundation hardware H.264 MFT: CBR, low
//!    latency (no B-frames), Annex-B with SPS/PPS in band, a keyframe every
//!    4 s of wall time plus forced keyframes on request.
//! 5. **Deliver** each access unit as a [`tincan_core::media::VideoFrame`]
//!    through `try_send`. A full channel drops the frame instead of queueing,
//!    then skips deltas to the next (forced) keyframe.
//!
//! A window resize or display mode change reopens capture and rebuilds the
//! encoder at the new size (the next frame is a keyframe with new SPS). A
//! closed window or removed display ends the share; [`ScreenShare::ended`]
//! reports why.
//!
//! Modes and bitrates (from `docs/architecture.md`):
//!
//! | Mode | Frame cap | Start bitrate | Cap | Normal-tier area |
//! |---|---|---|---|---|
//! | [`ShareMode::Text`] (default) | 15 fps | 1.2 Mbps | 2.5 Mbps | 1920x1200 |
//! | [`ShareMode::Motion`] | 30 fps | 3 Mbps | 4.5 Mbps | 1920x1080 |
//!
//! [`ScreenShare::set_bitrate`] clamps to 300 kbps ..= cap. Below 600 kbps a
//! share drops to 720p-class at 10 fps and returns at 750 kbps.
//!
//! There is no software fallback: without a hardware encoder
//! [`ScreenShare::start`] fails with [`Error::NoHardwareEncoder`], which the
//! app reports as `no-hardware-encoder`.
//!
//! Windows only (x86_64-pc-windows-msvc). Idle cost is zero: nothing runs
//! until a share starts, and a share's threads block on events.
//!
//! Process-wide side effect: the first share calls `CoIncrementMTAUsage` once
//! and never releases it. windows-rs caches WinRT factories in statics, and
//! without a live MTA between shares a second share crashed inside
//! Windows.Graphics.Capture (reproduced). It costs nothing and creates no
//! thread.
#![warn(missing_docs)]

mod error;
mod policy;
mod probe;
mod share;
mod source;

pub use error::{Error, Result};
pub use policy::{
    GOP, KEYFRAME_MIN_INTERVAL, LOW_TIER_ENTER_BPS, LOW_TIER_EXIT_BPS, LOW_TIER_FPS, MAX_ENCODE_WIDTH, MIN_BITRATE_BPS,
    ShareMode,
};
pub use probe::hardware_encoder_available;
pub use share::{EndReason, ScreenShare, ShareStats};
pub use source::{MIN_WINDOW_SIDE, ScreenSource, SourceKind, sources};
