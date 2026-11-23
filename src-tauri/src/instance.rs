//! One instance per profile. Decided in `main` before the runtime or any window exists, with
//! two named events in the session's `Local\` namespace, keyed by the lowercased profile
//! name (profile folders are case-insensitive, kernel object names are not):
//!
//! - `open` (auto-reset) exists while an instance of the profile is up and not closing.
//!   Creating it elects this process. A launch that finds it signals it and exits with 0
//!   and no window; the running instance then brings its window to the front.
//! - `alive` (never signalled) exists until the process of an instance ends. It outlives
//!   `open` by the close: the window is hidden, the host handoff runs and the profile lock is
//!   still held. A launch that wins `open` while `alive` still exists waits until that
//!   process is gone (capped), then starts.
//!
//! The objects only steer launches; the profile lock in tincan-app still guards the data.
//! When they cannot be created the launch goes ahead unchecked.

use std::time::{Duration, Instant};

/// How often a waiting launch checks whether the previous instance has ended.
const POLL: Duration = Duration::from_millis(100);

/// Name of the kernel object `role` of `profile`.
pub fn object_name(profile: &str, role: &str) -> String {
    format!("Local\\Tincan.{}.{role}", profile.to_ascii_lowercase())
}

/// What a launch found.
pub enum Claim {
    /// Another instance of the profile is up and was asked to come to the front.
    Focused,
    /// This process is the profile's instance.
    Primary(Primary),
    /// The named objects could not be used; start without the check.
    Unchecked,
}

/// Claims `profile` for this process. Waits up to `cap` while a previous instance is closing.
pub fn claim(profile: &str, cap: Duration) -> Claim {
    claim_names(&object_name(profile, "open"), &object_name(profile, "alive"), cap)
}

fn claim_names(open_name: &str, alive_name: &str, cap: Duration) -> Claim {
    let open = match sys::create_event(Some(open_name), false) {
        Ok((open, false)) => open,
        Ok((running, true)) => {
            sys::allow_any_foreground();
            sys::signal(&running);
            return Claim::Focused;
        }
        Err(code) => {
            eprintln!("tincan: couldn't check for another instance of this profile (error {code})");
            return Claim::Unchecked;
        }
    };
    let started = Instant::now();
    let deadline = started + cap;
    let mut waited = false;
    while sys::exists(alive_name) && Instant::now() < deadline {
        waited = true;
        std::thread::sleep(POLL);
    }
    if waited {
        let gone = !sys::exists(alive_name);
        eprintln!(
            "tincan: waited {:?} for the previous instance of this profile to close{}",
            started.elapsed(),
            if gone { "" } else { "; it is still running" }
        );
    }
    let alive = sys::create_event(Some(alive_name), true).ok().map(|(h, _)| h);
    Claim::Primary(Primary { open, alive })
}

/// The profile's instance, before its window exists. Dropping it frees the profile for the
/// next launch.
pub struct Primary {
    open: sys::Handle,
    alive: Option<sys::Handle>,
}

impl Primary {
    /// Starts answering launches of the profile: `on_focus` runs on a watcher thread for each
    /// one, until [`Instance::release`]. A launch that came before this call is answered now.
    pub fn watch(self, on_focus: impl Fn() + Send + 'static) -> Instance {
        let Primary { open, alive } = self;
        let stop = match sys::create_event(None, true) {
            Ok((stop, _)) => std::sync::Arc::new(stop),
            Err(code) => {
                eprintln!("tincan: couldn't watch for other launches of this profile (error {code})");
                return Instance { stop: None, _alive: alive };
            }
        };
        let thread_stop = stop.clone();
        let spawned = std::thread::Builder::new().name("tincan-instance".into()).spawn(move || {
            while sys::wait_first(&open, &thread_stop) {
                on_focus();
            }
            // the last handle of `open` closes here, on the thread that waited on it
            drop(open);
        });
        if let Err(e) = spawned {
            eprintln!("tincan: couldn't watch for other launches of this profile ({e})");
            return Instance { stop: None, _alive: alive };
        }
        Instance { stop: Some(stop), _alive: alive }
    }
}

/// The running instance of the profile. Keep it for the life of the process.
pub struct Instance {
    stop: Option<std::sync::Arc<sys::Handle>>,
    _alive: Option<sys::Handle>,
}

impl Instance {
    /// The instance is closing: from now on a launch of the profile starts a new instance
    /// (which waits for this process to end) instead of asking this one to come forward.
    pub fn release(&mut self) {
        if let Some(stop) = self.stop.take() {
            sys::signal(&stop);
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::ptr::null;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, FALSE, GetLastError, HANDLE, SetLastError, TRUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, INFINITE, OpenEventW, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};

    /// An owned kernel handle, closed on drop.
    pub struct Handle(HANDLE);

    // SAFETY: a kernel event handle may be used and closed from any thread.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle is owned and closed exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// Creates (or opens) an event; unnamed when `name` is `None`. Returns the handle and
    /// whether a named event of that name existed already, or the Win32 error code.
    pub fn create_event(name: Option<&str>, manual_reset: bool) -> Result<(Handle, bool), u32> {
        let name = name.map(wide);
        let ptr = name.as_ref().map_or(null(), |n| n.as_ptr());
        // SAFETY: plain Win32 calls; `ptr` is null or a NUL-terminated UTF-16 string that
        // outlives the call. The last error is read right after the call that sets it.
        unsafe {
            SetLastError(0);
            let h = CreateEventW(null(), if manual_reset { TRUE } else { FALSE }, FALSE, ptr);
            let err = GetLastError();
            if h.is_null() { Err(err) } else { Ok((Handle(h), err == ERROR_ALREADY_EXISTS)) }
        }
    }

    /// True while an event of this name exists. The probe handle is closed at once, so the
    /// probe itself never keeps the object alive.
    pub fn exists(name: &str) -> bool {
        let name = wide(name);
        // SAFETY: `name` is NUL-terminated and outlives the call; the handle is closed here.
        unsafe {
            let h = OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, FALSE, name.as_ptr());
            if h.is_null() {
                // anything but "not found" (no access, another object type) counts as taken
                return GetLastError() != ERROR_FILE_NOT_FOUND;
            }
            CloseHandle(h);
            true
        }
    }

    pub fn signal(event: &Handle) {
        // SAFETY: a valid event handle.
        unsafe { SetEvent(event.0) };
    }

    /// Waits for `event` or `stop`. True when `event` fired and `stop` has not.
    pub fn wait_first(event: &Handle, stop: &Handle) -> bool {
        let handles = [event.0, stop.0];
        // SAFETY: two valid handles that outlive the waits.
        unsafe {
            WaitForMultipleObjects(2, handles.as_ptr(), FALSE, INFINITE) == WAIT_OBJECT_0 && WaitForSingleObject(stop.0, 0) != WAIT_OBJECT_0
        }
    }

    /// Lets the running instance take the foreground. A launch the user just started may do
    /// that and may pass it on; the running instance may not do it on its own.
    pub fn allow_any_foreground() {
        // SAFETY: plain Win32 call.
        unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    }
}

#[cfg(not(windows))]
mod sys {
    //! Other platforms run every launch unchecked.
    pub struct Handle;
    pub fn create_event(_: Option<&str>, _: bool) -> Result<(Handle, bool), u32> {
        Err(0)
    }
    pub fn exists(_: &str) -> bool {
        false
    }
    pub fn signal(_: &Handle) {}
    pub fn wait_first(_: &Handle, _: &Handle) -> bool {
        false
    }
    pub fn allow_any_foreground() {}
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::SystemTime;

    /// Object names no other test or running Tincan uses.
    fn names(tag: &str) -> (String, String) {
        let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
        let key = format!("e2e-shell-test-{tag}-{}-{nanos}", std::process::id());
        (object_name(&key, "open"), object_name(&key, "alive"))
    }

    fn primary(c: Claim) -> Primary {
        match c {
            Claim::Primary(p) => p,
            Claim::Focused => panic!("expected to be the primary, got Focused"),
            Claim::Unchecked => panic!("expected to be the primary, got Unchecked"),
        }
    }

    fn wait_gone(name: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while sys::exists(name) {
            assert!(Instant::now() < deadline, "{name} still exists");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn names_are_per_session_and_ignore_case() {
        assert_eq!(object_name("e2e-A", "open"), "Local\\Tincan.e2e-a.open");
        assert_eq!(object_name("E2E-a", "open"), object_name("e2e-a", "open"));
        assert_ne!(object_name("e2e-a", "open"), object_name("e2e-b", "open"));
        assert_ne!(object_name("e2e-a", "open"), object_name("e2e-a", "alive"));
    }

    #[test]
    fn a_second_launch_brings_the_first_forward() {
        let (open, alive) = names("focus");
        let first = primary(claim_names(&open, &alive, Duration::from_secs(1)));
        let (tx, rx) = mpsc::channel();
        let mut instance = first.watch(move || tx.send(()).unwrap());

        for _ in 0..2 {
            let t = Instant::now();
            assert!(matches!(claim_names(&open, &alive, Duration::from_secs(5)), Claim::Focused));
            assert!(t.elapsed() < Duration::from_millis(500), "a second launch does not wait");
            rx.recv_timeout(Duration::from_secs(2)).expect("the first instance was asked to come forward");
        }
        instance.release();
        wait_gone(&open);
        assert!(rx.try_recv().is_err(), "releasing is not a focus request");
    }

    #[test]
    fn a_launch_before_the_watcher_starts_is_answered() {
        let (open, alive) = names("early");
        let first = primary(claim_names(&open, &alive, Duration::from_secs(1)));
        assert!(matches!(claim_names(&open, &alive, Duration::from_secs(1)), Claim::Focused));
        let (tx, rx) = mpsc::channel();
        let mut instance = first.watch(move || tx.send(()).unwrap());
        rx.recv_timeout(Duration::from_secs(2)).expect("the pending request was answered");
        instance.release();
    }

    #[test]
    fn a_launch_during_the_close_waits_for_the_previous_process() {
        let (open, alive) = names("close");
        let mut closing = primary(claim_names(&open, &alive, Duration::from_secs(1))).watch(|| panic!("no focus request"));
        closing.release();
        wait_gone(&open);
        assert!(sys::exists(&alive), "the closing instance is still alive");

        // the previous process ends 300 ms later
        let ends = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(closing);
        });
        let t = Instant::now();
        let next = primary(claim_names(&open, &alive, Duration::from_secs(5)));
        let waited = t.elapsed();
        ends.join().unwrap();
        assert!(waited >= Duration::from_millis(250), "started after {waited:?}, before the previous instance ended");
        assert!(waited < Duration::from_secs(3), "waited {waited:?}");

        // the new instance is the one a further launch reaches
        assert!(matches!(claim_names(&open, &alive, Duration::from_secs(1)), Claim::Focused));
        drop(next);
        wait_gone(&open);
        wait_gone(&alive);
    }

    #[test]
    fn a_previous_instance_that_never_ends_is_waited_for_at_most_the_cap() {
        let (open, alive) = names("stuck");
        let mut stuck = primary(claim_names(&open, &alive, Duration::from_secs(1))).watch(|| {});
        stuck.release();
        wait_gone(&open);
        let t = Instant::now();
        let next = primary(claim_names(&open, &alive, Duration::from_millis(300)));
        let waited = t.elapsed();
        assert!(waited >= Duration::from_millis(300) && waited < Duration::from_secs(2), "waited {waited:?}");
        drop(next);
        drop(stuck);
    }

    #[test]
    fn a_name_taken_by_another_object_type_runs_unchecked() {
        let (open, alive) = names("taken");
        // a mutex under the event's name: CreateEventW fails with ERROR_INVALID_HANDLE
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let wide: Vec<u16> = open.encode_utf16().chain(Some(0)).collect();
        // SAFETY: plain Win32 calls with a NUL-terminated name; the handle is closed below.
        let m = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        assert!(!m.is_null());
        assert!(matches!(claim_names(&open, &alive, Duration::from_millis(100)), Claim::Unchecked));
        // SAFETY: closes the mutex created above.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(m) };
    }
}
