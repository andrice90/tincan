//! The exit sequence's state and its bounded shutdown, shared by every way the process ends:
//! a close request (title bar, `window_close`, Alt+F4), a start-up failure, and Windows
//! ending the session (logoff, shutdown, Restart Manager).
//!
//! On Windows the end of the session does not come as a close request. tao handles
//! `WM_ENDSESSION` by destroying the event loop (`RunEvent::Exit`) and calling
//! `std::process::exit` as soon as that event returns, so the shell has to run
//! `App::shutdown` inside `RunEvent::Exit`, blocking, within the few seconds Windows allows.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::gate::Gate;

/// Where the exit sequence stands.
#[derive(Debug, Default)]
pub struct ExitState {
    /// The exit sequence started (close request, start-up failure or end of session).
    closing: AtomicBool,
    /// Tincan asked the event loop to end (`exit_now`); the shutdown already ran or gave up.
    exiting: AtomicBool,
}

impl ExitState {
    /// Starts the exit sequence. True for the first caller only, which then runs it.
    pub fn begin(&self) -> bool {
        !self.closing.swap(true, Ordering::AcqRel)
    }

    /// From here on the event loop ends because Tincan asked it to.
    pub fn mark_exiting(&self) {
        self.exiting.store(true, Ordering::Release);
    }

    /// The event loop is being destroyed (`RunEvent::Exit`). Returns true when Tincan did not
    /// ask for that (Windows is ending the session), so the caller must still shut the App
    /// down before returning. Also marks the exit sequence as started, so a start-up failure
    /// that lands now opens no dialog.
    pub fn loop_destroyed(&self) -> bool {
        self.closing.store(true, Ordering::Release);
        !self.exiting.load(Ordering::Acquire)
    }
}

/// Waits for a start in progress, then runs `shutdown` on the started value; gives up after
/// `cap`. Returns false when it gave up.
///
/// The shutdown runs in its own task, so giving up never cancels it half-way. `App::shutdown`
/// runs once and concurrent callers wait, so a later call joins the run still in progress
/// instead of starting it over. A start that failed (or a gate that never opens) means there
/// is nothing to shut down.
pub async fn shutdown_within<T, F, Fut>(gate: &Gate<T>, cap: Duration, shutdown: F) -> bool
where
    T: Clone,
    F: FnOnce(T) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let run = async {
        if let Ok(started) = gate.get().await {
            let _ = tokio::spawn(shutdown(started)).await;
        }
    };
    tokio::time::timeout(cap, run).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::Phase;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    #[test]
    fn own_exit_needs_no_shutdown_when_the_loop_ends() {
        let s = ExitState::default();
        assert!(s.begin());
        s.mark_exiting();
        assert!(!s.loop_destroyed());
    }

    #[test]
    fn end_of_session_needs_the_shutdown_and_blocks_dialogs() {
        // nothing started: Windows ended the session
        let s = ExitState::default();
        assert!(s.loop_destroyed());
        assert!(!s.begin(), "a start-up failure after this shows no dialog");

        // a close was in progress but had not finished: join it
        let s = ExitState::default();
        assert!(s.begin());
        assert!(s.loop_destroyed());
    }

    #[test]
    fn begin_runs_once() {
        let s = ExitState::default();
        assert!(s.begin());
        assert!(!s.begin());
    }

    fn counting() -> (Arc<AtomicUsize>, impl Fn() -> Arc<AtomicUsize>) {
        let n = Arc::new(AtomicUsize::new(0));
        let clone = n.clone();
        (n, move || clone.clone())
    }

    #[tokio::test]
    async fn shuts_down_a_started_value() {
        let (tx, gate) = Gate::<u32>::new();
        tx.send_replace(Phase::Ready(7));
        let (runs, handle) = counting();
        let ok = shutdown_within(&gate, Duration::from_secs(1), |v| {
            let runs = handle();
            async move {
                assert_eq!(v, 7);
                runs.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;
        assert!(ok);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn waits_for_a_start_in_progress() {
        let (tx, gate) = Gate::<u32>::new();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            tx.send_replace(Phase::Ready(1));
            // keep the sender alive a little longer, as `start` does
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        let (runs, handle) = counting();
        let ok = shutdown_within(&gate, Duration::from_secs(2), |_| {
            let runs = handle();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;
        assert!(ok);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn nothing_to_do_after_a_failed_or_abandoned_start() {
        let (tx, gate) = Gate::<u32>::new();
        tx.send_replace(Phase::Failed("profile in use".into()));
        assert!(shutdown_within(&gate, Duration::from_millis(100), |_| async { panic!("no shutdown") }).await);

        let (tx, gate) = Gate::<u32>::new();
        drop(tx);
        assert!(shutdown_within(&gate, Duration::from_millis(100), |_| async { panic!("no shutdown") }).await);
    }

    #[tokio::test]
    async fn giving_up_does_not_cancel_the_shutdown() {
        let (tx, gate) = Gate::<u32>::new();
        tx.send_replace(Phase::Ready(1));
        let (done, handle) = counting();
        let t = Instant::now();
        let ok = shutdown_within(&gate, Duration::from_millis(50), |_| {
            let done = handle();
            async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                done.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await;
        assert!(!ok);
        assert!(t.elapsed() < Duration::from_millis(190), "gave up at the cap");
        assert_eq!(done.load(Ordering::SeqCst), 0);
        // the shutdown task keeps going and finishes on its own
        let deadline = Instant::now() + Duration::from_secs(5);
        while done.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "the shutdown was cancelled");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_never_opening_gate_is_bounded() {
        let (_tx, gate) = Gate::<u32>::new();
        let t = Instant::now();
        assert!(!shutdown_within(&gate, Duration::from_millis(50), |_| async {}).await);
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    /// The end-of-session path with the real App, the way `RunEvent::Exit` runs it: Alice
    /// hosts a server Bob is in, and her shutdown runs blocking on the event loop's thread
    /// (outside the runtime). It finishes within the cap with a clean handoff, so Bob hosts
    /// at once instead of after the 10 s of silence a killed host costs. The profile lock is
    /// released at the end of the shutdown, so Alice's profile starts again afterwards.
    #[test]
    fn end_of_session_hands_the_server_off_from_outside_the_runtime() {
        use serde_json::{Value, json};
        use tincan_app::{App, AppConfig, TestOptions};

        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(3).enable_all().build().unwrap();
        let cfg = |dir: &tempfile::TempDir| AppConfig {
            data_dir: dir.path().to_path_buf(),
            app_version: "t".into(),
            file_url_base: crate::protocol::file_url_base(),
            test: TestOptions { offline_network: true, no_devices: true },
        };
        async fn call(app: &App, cmd: &str, args: Value) -> Value {
            match tokio::time::timeout(Duration::from_secs(30), app.call(cmd, args)).await {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => panic!("{cmd} failed: {e}"),
                Err(_) => panic!("{cmd} timed out"),
            }
        }
        async fn wait_server(rx: &mut tokio::sync::broadcast::Receiver<tincan_app::UiEvent>, secs: u64, pred: impl Fn(&Value) -> bool) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
            loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Ok(ev)) if ev.name == "server-updated" && pred(&ev.payload) => return,
                    Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                    Ok(Err(e)) => panic!("event stream ended: {e}"),
                    Err(_) => panic!("no matching server-updated within {secs} s"),
                }
            }
        }

        let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let alice = rt.block_on(App::start(cfg(&dir_a))).expect("start Alice");
        assert_eq!(rt.block_on(App::start(cfg(&dir_a))).err().as_deref(), Some("Tincan is already running with this profile."));
        let bob = rt.block_on(App::start(cfg(&dir_b))).expect("start Bob");
        let mut bob_rx = bob.subscribe();
        let (alice_id, bob_id, server) = rt.block_on(async {
            let a = call(&alice, "set_name", json!({ "name": "Alice" })).await["userId"].as_str().unwrap().to_string();
            let b = call(&bob, "set_name", json!({ "name": "Bob" })).await["userId"].as_str().unwrap().to_string();
            let server = call(&alice, "create_server", json!({ "name": "Club" })).await["id"].as_str().unwrap().to_string();
            let invite = call(&alice, "create_invite", json!({ "serverId": server })).await;
            call(&bob, "join_server", json!({ "invite": invite })).await;
            wait_server(&mut bob_rx, 30, |s| s["id"] == server.as_str() && s["connection"]["state"] == "member" && s["connection"]["hostId"] == a.as_str())
                .await;
            // let statuses and the succession list settle
            tokio::time::sleep(Duration::from_secs(2)).await;
            (a, b, server)
        });

        let (tx, gate) = Gate::<Arc<App>>::new();
        tx.send_replace(Phase::Ready(alice));
        let handle = rt.handle().clone();
        let t = Instant::now();
        let ok = std::thread::spawn(move || {
            handle.block_on(shutdown_within(&gate, crate::END_SESSION_CAP, |app: Arc<App>| async move { app.shutdown().await }))
        })
        .join()
        .unwrap();
        let took = t.elapsed();
        assert!(ok, "the shutdown finished within {:?}", crate::END_SESSION_CAP);

        rt.block_on(wait_server(&mut bob_rx, 5, |s| {
            s["id"] == server.as_str()
                && s["connection"]["hostId"] == bob_id.as_str()
                && matches!(s["connection"]["state"].as_str(), Some("solo" | "hosting"))
        }));
        let handed_off = t.elapsed();
        eprintln!("end-of-session shutdown took {took:?}; Bob hosts {handed_off:?} after it started");
        assert!(handed_off < Duration::from_secs(8), "Bob hosts {handed_off:?} after Alice's shutdown started (took {took:?})");
        assert_ne!(alice_id, bob_id);

        let again = rt.block_on(App::start(cfg(&dir_a))).expect("Alice's profile lock was released");
        rt.block_on(again.shutdown());
        rt.block_on(bob.shutdown());
    }
}
