//! Shareable sources: displays and top-level windows.

use moq_video::capture;

use crate::{Error, Result};

/// Windows smaller than this (either side, pixels) are not offered: they are
/// tray popups, tooltips and other slivers nobody means to share.
pub const MIN_WINDOW_SIDE: u32 = 64;

/// Process image name (without `.exe`) whose windows are never offered.
const OWN_APP: &str = "tincan";

/// Whether a source is a whole display or a single window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// A whole monitor.
    Display,
    /// One top-level window.
    Window,
}

impl SourceKind {
    /// The UI/API string for this kind: `"display"` or `"window"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            SourceKind::Display => "display",
            SourceKind::Window => "window",
        }
    }
}

/// Something the user can share, as listed by [`sources`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenSource {
    /// Opaque id to pass to [`ScreenShare::start`](crate::ScreenShare::start):
    /// `display:<index>` (index 0 is the primary display) or `window:<HWND>`.
    /// Window ids are valid only while that window exists; display indices can
    /// shift when monitors are attached or removed.
    pub id: String,
    /// Display or window.
    pub kind: SourceKind,
    /// Human-readable label: `Display 1 (primary)`, or the window title.
    pub name: String,
    /// Width in physical pixels (per-monitor DPI aware).
    pub width: u32,
    /// Height in physical pixels (per-monitor DPI aware).
    pub height: u32,
}

/// List the shareable sources: every display first (the primary display
/// first of all), then the visible top-level windows in z-order.
///
/// Skips windows that are invisible, cloaked, untitled, minimized-to-nothing or
/// smaller than [`MIN_WINDOW_SIDE`], tool windows, the desktop itself, and
/// Tincan's own windows (process `tincan.exe` or this process). If windows
/// cannot be enumerated the displays are still returned.
pub async fn sources() -> Result<Vec<ScreenSource>> {
    let displays = capture::displays().await.map_err(Error::from_capture)?;
    let count = displays.len();
    let mut out: Vec<ScreenSource> = displays
        .into_iter()
        .enumerate()
        .filter(|(_, d)| d.width > 0 && d.height > 0)
        .map(|(index, d)| ScreenSource {
            id: d.id,
            kind: SourceKind::Display,
            name: display_name(index, count),
            width: d.width,
            height: d.height,
        })
        .collect();

    match capture::windows().await {
        Ok(windows) => {
            let own_pid = std::process::id();
            for w in windows {
                if !offer_window(&w, own_pid) {
                    continue;
                }
                out.push(ScreenSource {
                    id: w.id,
                    kind: SourceKind::Window,
                    name: w.title,
                    width: w.width,
                    height: w.height,
                });
            }
        }
        Err(err) => tracing::warn!(%err, "could not enumerate windows; offering displays only"),
    }
    Ok(out)
}

/// `Display 1`, with ` (primary)` on the first entry when there are several.
fn display_name(index: usize, count: usize) -> String {
    if index == 0 && count > 1 {
        "Display 1 (primary)".to_owned()
    } else {
        format!("Display {}", index + 1)
    }
}

fn offer_window(w: &capture::Window, own_pid: u32) -> bool {
    if w.width < MIN_WINDOW_SIDE || w.height < MIN_WINDOW_SIDE || w.title.trim().is_empty() {
        return false;
    }
    if w.app.eq_ignore_ascii_case(OWN_APP) {
        return false;
    }
    // The desktop wallpaper window ("Program Manager") is a display, not a window.
    if w.app.eq_ignore_ascii_case("explorer") && w.title == "Program Manager" {
        return false;
    }
    match win32::inspect(&w.id) {
        Some(info) => info.pid != own_pid && !info.tool_window,
        None => false,
    }
}

/// Translate a [`ScreenSource::id`] into the capture selector it names.
pub(crate) fn parse_id(id: &str) -> Result<(SourceKind, capture::Source)> {
    if let Some(rest) = id.strip_prefix("display:") {
        if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) && rest.parse::<usize>().is_ok() {
            return Ok((SourceKind::Display, capture::Source::Display(Some(id.to_owned()))));
        }
    } else if let Some(rest) = id.strip_prefix("window:")
        && rest.bytes().all(|b| b.is_ascii_digit())
        && rest.parse::<usize>().is_ok_and(|h| h != 0)
    {
        return Ok((SourceKind::Window, capture::Source::Window(id.to_owned())));
    }
    Err(Error::InvalidSource(id.to_owned()))
}

/// Whether the window behind a `window:<HWND>` id still exists. Always true
/// for display ids and on hosts without Win32.
///
/// WGC is supposed to raise its item `Closed` event when a captured window is
/// destroyed, but it was observed not to (the capture kept repeating the last
/// picture), so the share loop checks this itself.
pub(crate) fn window_exists(id: &str) -> bool {
    match id.strip_prefix("window:").and_then(|raw| raw.parse::<usize>().ok()) {
        Some(raw) => win32::is_window(raw),
        None => true,
    }
}

#[cfg(windows)]
mod win32 {
    use std::ffi::c_void;

    use moq_video::windows::Win32::Foundation::HWND;
    use moq_video::windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, GetWindowThreadProcessId, IsWindow, WS_EX_TOOLWINDOW,
    };

    /// Whether `raw` is a live window handle.
    pub(super) fn is_window(raw: usize) -> bool {
        // SAFETY: IsWindow accepts any value and only validates the handle.
        unsafe { IsWindow(Some(HWND(raw as *mut c_void))) }.as_bool()
    }

    /// Owner process and style of a listed window.
    pub(super) struct Info {
        pub pid: u32,
        pub tool_window: bool,
    }

    /// Read the owner pid and extended style of `window:<HWND>`. None if the id
    /// is malformed or the window is already gone.
    pub(super) fn inspect(id: &str) -> Option<Info> {
        let raw: usize = id.strip_prefix("window:")?.parse().ok()?;
        let hwnd = HWND(raw as *mut c_void);
        let mut pid = 0u32;
        // SAFETY: both calls accept any HWND value and only read window state;
        // a stale handle makes them return 0 rather than touching memory.
        let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if thread == 0 {
            return None;
        }
        // SAFETY: as above.
        let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
        Some(Info {
            pid,
            tool_window: (ex_style as u32) & WS_EX_TOOLWINDOW.0 != 0,
        })
    }
}

#[cfg(not(windows))]
mod win32 {
    pub(super) struct Info {
        pub pid: u32,
        pub tool_window: bool,
    }

    pub(super) fn inspect(_id: &str) -> Option<Info> {
        Some(Info {
            pid: 0,
            tool_window: false,
        })
    }

    pub(super) fn is_window(_raw: usize) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_parse_to_capture_selectors() {
        let (kind, source) = parse_id("display:0").unwrap();
        assert_eq!(kind, SourceKind::Display);
        assert_eq!(source, capture::Source::Display(Some("display:0".into())));
        let (kind, source) = parse_id("window:1234").unwrap();
        assert_eq!(kind, SourceKind::Window);
        assert_eq!(source, capture::Source::Window("window:1234".into()));
    }

    #[test]
    fn malformed_ids_are_rejected() {
        for bad in [
            "",
            "display:",
            "display:-1",
            "display:x",
            "window:",
            "window:0",
            "window:-5",
            "window:12a",
            "1234",
            "screen:0",
            "display:1 ",
        ] {
            assert!(matches!(parse_id(bad), Err(Error::InvalidSource(_))), "{bad:?}");
        }
    }

    #[test]
    fn display_names_mark_the_primary_only_when_ambiguous() {
        assert_eq!(display_name(0, 1), "Display 1");
        assert_eq!(display_name(0, 2), "Display 1 (primary)");
        assert_eq!(display_name(1, 2), "Display 2");
    }

    #[test]
    fn kind_strings_match_the_ui_contract() {
        assert_eq!(SourceKind::Display.as_str(), "display");
        assert_eq!(SourceKind::Window.as_str(), "window");
    }
}
