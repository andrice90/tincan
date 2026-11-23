//! Host-election inputs from the operating system: AC power and process uptime.

use std::sync::OnceLock;
use std::time::Instant;

/// Is the machine on AC power? Desktops (no battery) and unknown states count as AC.
#[cfg(windows)]
pub(crate) fn ac_power() -> bool {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut status = SYSTEM_POWER_STATUS {
        ACLineStatus: 255,
        BatteryFlag: 0,
        BatteryLifePercent: 0,
        SystemStatusFlag: 0,
        BatteryLifeTime: 0,
        BatteryFullLifeTime: 0,
    };
    // SAFETY: `status` is a valid, writable SYSTEM_POWER_STATUS that outlives the call.
    let ok = unsafe { GetSystemPowerStatus(&mut status) };
    // ACLineStatus: 0 = offline (battery), 1 = online, 255 = unknown.
    ok == 0 || status.ACLineStatus != 0
}

/// Is the machine on AC power? (Non-Windows builds: assume yes.)
#[cfg(not(windows))]
pub(crate) fn ac_power() -> bool { true }

fn process_start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// Mark the process start (called by `Net::start`; the first call wins).
pub(crate) fn mark_start() { let _ = process_start(); }

/// Seconds since the process (network layer) started.
pub(crate) fn uptime_s() -> u32 { u32::try_from(process_start().elapsed().as_secs()).unwrap_or(u32::MAX) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_and_uptime_do_not_panic() {
        mark_start();
        let _ = ac_power();
        assert!(uptime_s() < 1_000_000);
    }
}
