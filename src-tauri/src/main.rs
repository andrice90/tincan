//! Tincan desktop app: the thin Tauri shell around `tincan-app` (architecture.md,
//! "Process model"). It owns the window, the profile choice, the `api` / `screen_watch` /
//! window commands, event forwarding, the `tincan` file scheme and the exit sequence.
//!
//! Environment:
//! - `TINCAN_PROFILE`: profile name when `--profile` is not given (default `default`).
//! - `TINCAN_OFFLINE_NETWORK=1`, `TINCAN_NO_DEVICES=1`: set the matching `TestOptions`
//!   (end-to-end runs of several profiles on one machine). Unset in normal use.
//! - `TINCAN_LOG=<filter>`: an `EnvFilter` such as `info` or `warn,tincan_audio=info`, for
//!   the log file and also stderr. Unset: the log file only, with `logfile::DEFAULT_FILTER`.
//!   The log is `%APPDATA%\Tincan\profiles\<profile>\logs\tincan.log`.
//!
//! Running: the Tauri CLI must start in the repository root (it finds `src-tauri` below
//! it), so `ui/package.json` has `"tauri": "cd .. && tauri"`. From `ui/`:
//! `TINCAN_PROFILE=e2e-a pnpm tauri dev` (Vite dev server, `cfg(dev)`), `pnpm tauri build`
//! (embeds `ui/dist`, NSIS installer). A plain `cargo build -p tincan` is a dev build that
//! loads `http://localhost:5173`; add `--features tauri/custom-protocol` to embed `ui/dist`.
//! Two instances: `tincan.exe --profile e2e-a` and `tincan.exe --profile e2e-b`.
//!
//! One instance per profile (`instance.rs`): launching a profile that is already open brings
//! its window to the front and exits with 0; a launch while it is closing waits for it.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod events;
mod exit;
mod gate;
mod instance;
mod logfile;
mod profile;
mod protocol;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::webview::PageLoadEvent;
use tauri::window::Color;
use tauri::{AppHandle, Manager, RunEvent, Runtime, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tincan_app::{App, AppConfig, TestOptions};

use crate::exit::ExitState;
use crate::gate::{Gate, GateSender, Phase};
use crate::instance::{Claim, Instance, Primary};

/// Label of the one window.
const MAIN_WINDOW: &str = "main";
/// `--bg-rail`, so the window never flashes white before the page paints.
const BACKGROUND: Color = Color(0x14, 0x16, 0x19, 0xff);
/// Upper bound for the clean shutdown (host handoff) before the process exits anyway.
const SHUTDOWN_CAP: Duration = Duration::from_secs(5);
/// Last resort if the event loop does not end after `exit`.
const EXIT_GRACE: Duration = Duration::from_secs(3);
/// Upper bound for the shutdown when Windows ends the session (logoff, shutdown, Restart
/// Manager). Windows waits about 5 s for an app to return from `WM_ENDSESSION` before it
/// offers to close it anyway.
pub(crate) const END_SESSION_CAP: Duration = Duration::from_secs(4);
/// How long a launch waits for a closing instance of its profile to end: that process exits
/// at most `SHUTDOWN_CAP + EXIT_GRACE` after its close started.
const PREVIOUS_INSTANCE_CAP: Duration = Duration::from_secs(9);

/// Shell state shared by commands, the file scheme and the exit sequence.
pub struct Shell {
    /// Resolves to the running App once `App::start` finished.
    pub gate: Gate<Arc<App>>,
    /// Counts page loads of the main window; a video sink created by an older page stops.
    pub page_loads: Arc<AtomicU64>,
    /// Where the exit sequence stands.
    exit: ExitState,
    /// This process as the profile's instance, once the window exists. `None` when the check
    /// was not possible.
    instance: Mutex<Option<Instance>>,
}

impl Shell {
    /// The exit sequence started: later launches of the profile start a new instance.
    fn release_instance(&self) {
        if let Some(instance) = self.instance.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            instance.release();
        }
    }
}

fn main() {
    // SAFETY: first statement of the process; no other thread exists yet, so nothing can read
    // the environment concurrently. iroh-blobs' FsStore sizes its own runtime from this.
    unsafe { std::env::set_var("TOKIO_WORKER_THREADS", "2") };
    let profile = profile::select(std::env::args_os().skip(1), std::env::var_os(profile::PROFILE_ENV));
    // Before the runtime and the window: a launch of a profile that is already open ends here.
    let primary = match &profile {
        Ok(name) => match instance::claim(name, PREVIOUS_INSTANCE_CAP) {
            Claim::Focused => {
                eprintln!("tincan: profile {name} is already open, bringing its window to the front");
                return;
            }
            Claim::Primary(primary) => Some(primary),
            Claim::Unchecked => None,
        },
        Err(_) => None,
    };
    // After the instance check: only the instance that runs the profile writes its log.
    init_logging(profile.as_ref().ok().map(String::as_str));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .thread_name("tincan-rt")
        .enable_all()
        .build()
        .expect("failed to build the tokio runtime");
    tauri::async_runtime::set(runtime.handle().clone());

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .register_asynchronous_uri_scheme_protocol(protocol::SCHEME, protocol::handle)
        .invoke_handler(tauri::generate_handler![
            commands::api,
            commands::screen_watch,
            commands::window_minimize,
            commands::window_toggle_maximize,
            commands::window_close,
        ])
        .setup(move |app| {
            setup(app, profile, primary);
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                begin_close(window.app_handle());
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build the Tauri app");

    app.run(|handle, event| match event {
        // The window never closes by itself (close requests are intercepted), so an exit
        // without a code means something else closed it: run the normal exit sequence.
        RunEvent::ExitRequested { code: None, api, .. } => {
            api.prevent_exit();
            begin_close(handle);
        }
        RunEvent::Exit => end_session(handle),
        _ => {}
    });
    drop(runtime);
}

/// Environment variable with an optional tracing filter.
const LOG_ENV: &str = "TINCAN_LOG";

/// Diagnostics: always to `<profile>/logs/tincan.log` (`logfile.rs`), filtered by
/// `TINCAN_LOG` or [`logfile::DEFAULT_FILTER`]; also to stderr when `TINCAN_LOG` is set.
fn init_logging(profile: Option<&str>) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, Layer, fmt};

    let env = std::env::var_os(LOG_ENV).and_then(|v| v.into_string().ok());
    let filter = |s: &str| match EnvFilter::try_new(s) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("tincan: ignoring {LOG_ENV}={s}: {e}");
            EnvFilter::new(logfile::DEFAULT_FILTER)
        }
    };
    let directives = env.clone().unwrap_or_else(|| logfile::DEFAULT_FILTER.to_string());
    let file = profile.zip(std::env::var_os("APPDATA")).and_then(|(name, roaming)| {
        let path = logfile::log_path(&profile::data_dir(std::path::Path::new(&roaming), name));
        match logfile::open(&path) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("tincan: no log file at {}: {e}", path.display());
                None
            }
        }
    });
    let file_layer = file.map(|f| {
        let f: &'static logfile::CappedFile = Box::leak(Box::new(f));
        fmt::layer().with_ansi(false).with_writer(move || f).with_filter(filter(&directives))
    });
    let stderr_layer = env.map(|d| fmt::layer().with_ansi(false).with_writer(std::io::stderr).with_filter(filter(&d)));
    let _ = tracing_subscriber::registry().with(file_layer).with(stderr_layer).try_init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "tincan started");
}

/// Creates the window at once and starts the App in the background.
fn setup(app: &mut tauri::App, profile: Result<String, String>, primary: Option<Primary>) {
    let (tx, gate) = Gate::new();
    app.manage(Shell { gate, page_loads: Arc::new(AtomicU64::new(0)), exit: ExitState::default(), instance: Mutex::new(None) });
    let handle = app.handle().clone();

    let name = match profile {
        Ok(name) => name,
        Err(e) => return fatal(&handle, e),
    };
    let data_dir = match app.path().data_dir() {
        Ok(roaming) => profile::data_dir(&roaming, &name),
        Err(e) => return fatal(&handle, format!("Couldn't find the AppData folder ({e}).")),
    };
    if let Err(e) = create_window(&handle, &name, &profile::webview_dir(&data_dir)) {
        return fatal(&handle, format!("Couldn't open the Tincan window ({e}). Check that Microsoft Edge WebView2 is installed."));
    }
    if let Some(primary) = primary {
        let focus = handle.clone();
        let instance = primary.watch(move || {
            let on_main = focus.clone();
            let _ = focus.run_on_main_thread(move || bring_to_front(&on_main));
        });
        *handle.state::<Shell>().instance.lock().unwrap_or_else(|e| e.into_inner()) = Some(instance);
    }

    let cfg = AppConfig {
        data_dir,
        app_version: app.package_info().version.to_string(),
        file_url_base: protocol::file_url_base(),
        test: TestOptions {
            offline_network: profile::env_flag(std::env::var_os("TINCAN_OFFLINE_NETWORK")),
            no_devices: profile::env_flag(std::env::var_os("TINCAN_NO_DEVICES")),
        },
    };
    tauri::async_runtime::spawn(start(handle, tx, cfg));
}

fn create_window<R: Runtime>(handle: &AppHandle<R>, profile: &str, webview_dir: &std::path::Path) -> tauri::Result<()> {
    let page_loads = handle.state::<Shell>().page_loads.clone();
    WebviewWindowBuilder::new(handle, MAIN_WINDOW, WebviewUrl::App("index.html".into()))
        .title(profile::window_title(profile))
        .inner_size(1280.0, 800.0)
        .min_inner_size(800.0, 520.0)
        .center()
        .decorations(false)
        .background_color(BACKGROUND)
        .data_directory(webview_dir.to_path_buf())
        .on_page_load(move |_, payload| {
            if payload.event() == PageLoadEvent::Started {
                page_loads.fetch_add(1, Ordering::AcqRel);
            }
        })
        .build()?;
    Ok(())
}

/// Runs `App::start`, wires event forwarding, then opens the gate; on failure shows the
/// error and exits.
async fn start<R: Runtime>(handle: AppHandle<R>, tx: GateSender<Arc<App>>, cfg: AppConfig) {
    let started = match tauri::async_runtime::spawn(App::start(cfg)).await {
        Ok(result) => result,
        Err(e) => Err(format!("Tincan failed to start ({e}).")),
    };
    match started {
        Ok(app) => {
            events::spawn_forwarder(&app, handle);
            tx.send_replace(Phase::Ready(app));
        }
        Err(e) => {
            tx.send_replace(Phase::Failed(e.clone()));
            fatal(&handle, e);
        }
    }
}

/// Another launch of the profile asked for the window: show it in front, unless it is
/// closing. Runs on the main thread, so it is ordered with `begin_close`'s hide.
fn bring_to_front<R: Runtime>(handle: &AppHandle<R>) {
    if handle.try_state::<Shell>().is_none_or(|s| s.exit.is_closing()) {
        return;
    }
    if let Some(window) = handle.get_webview_window(MAIN_WINDOW) {
        // tao's set_focus does nothing on a minimized window
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Start-up failed: hide the window, show `message` in a native message box, exit with 1.
fn fatal<R: Runtime>(handle: &AppHandle<R>, message: String) {
    eprintln!("tincan: {message}");
    let shell = handle.state::<Shell>();
    if !shell.exit.begin() {
        // the user is already closing the app: no dialog
        return;
    }
    shell.release_instance();
    if let Some(window) = handle.get_webview_window(MAIN_WINDOW) {
        let _ = window.hide();
    }
    let exit = handle.clone();
    handle
        .dialog()
        .message(message)
        .title("Tincan")
        .kind(MessageDialogKind::Error)
        .buttons(MessageDialogButtons::Ok)
        .show(move |_| exit_now(&exit, 1));
}

async fn shutdown_app(app: Arc<App>) {
    app.shutdown().await;
}

/// The exit sequence for every way of closing (title bar, `window_close`, Alt+F4): hide the
/// window at once, await `App::shutdown` (host handoff) for at most 5 s, then exit.
pub fn begin_close<R: Runtime>(handle: &AppHandle<R>) {
    let Some(shell) = handle.try_state::<Shell>() else {
        return exit_now(handle, 0);
    };
    if !shell.exit.begin() {
        return;
    }
    // before the hide: from now on a launch starts a new instance instead of showing this one
    shell.release_instance();
    if let Some(window) = handle.get_webview_window(MAIN_WINDOW) {
        let _ = window.hide();
    }
    let gate = shell.gate.clone();
    let handle = handle.clone();
    tauri::async_runtime::spawn(async move {
        // a close during start-up waits for the start (inside the same cap)
        if !exit::shutdown_within(&gate, SHUTDOWN_CAP, shutdown_app).await {
            eprintln!("tincan: shutdown took longer than {SHUTDOWN_CAP:?}, exiting anyway");
        }
        exit_now(&handle, 0);
    });
}

/// `RunEvent::Exit`: the event loop is being destroyed. After `exit_now` there is nothing
/// left to do. Otherwise Windows is ending the session (logoff, shutdown, Restart Manager):
/// tao turns `WM_ENDSESSION` into this event, with no close request before it, and exits the
/// process as soon as it returns. So the shutdown (host handoff, store flush) runs here,
/// blocking, for at most [`END_SESSION_CAP`]; a close already in progress is joined.
fn end_session<R: Runtime>(handle: &AppHandle<R>) {
    let Some(shell) = handle.try_state::<Shell>() else {
        return;
    };
    if !shell.exit.loop_destroyed() {
        return;
    }
    eprintln!("tincan: the session is ending, shutting down");
    shell.release_instance();
    let gate = shell.gate.clone();
    if !tauri::async_runtime::block_on(exit::shutdown_within(&gate, END_SESSION_CAP, shutdown_app)) {
        eprintln!("tincan: shutdown took longer than {END_SESSION_CAP:?}, exiting anyway");
    }
}

/// Ends the event loop; kills the process if that does not happen within [`EXIT_GRACE`].
fn exit_now<R: Runtime>(handle: &AppHandle<R>, code: i32) {
    if let Some(shell) = handle.try_state::<Shell>() {
        shell.exit.mark_exiting();
    }
    std::thread::spawn(move || {
        std::thread::sleep(EXIT_GRACE);
        std::process::exit(code);
    });
    handle.exit(code);
}
