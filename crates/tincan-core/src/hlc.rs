//! Hybrid logical clock: `(unix_ms << 16) | logical`, monotonic per process.
//!
//! `Hlc::now` returns strictly increasing values even if the wall clock goes
//! backwards; `Hlc::observe` folds in a remote hlc (Kulkarni et al. 2014) so a
//! later local event sorts after anything seen. Remote values more than
//! `state::MAX_FUTURE_MS` ahead of the local wall clock are ignored, so one peer
//! with a broken clock cannot drag everyone's clock into the future.

use parking_lot::Mutex;

/// Bits of the logical counter in an hlc value.
pub const LOGICAL_BITS: u32 = 16;

/// A hybrid logical clock (thread safe, cheap to share behind an `Arc`).
#[derive(Debug, Default)]
pub struct Hlc {
    last: Mutex<u64>,
}

impl Hlc {
    /// A clock starting at the current wall time.
    pub fn new() -> Self { Self::default() }
    /// Start no lower than `floor` (e.g. the highest own hlc found in storage).
    pub fn with_floor(floor: u64) -> Self { Self { last: Mutex::new(floor) } }
    /// Next timestamp for a local event.
    pub fn now(&self) -> u64 { self.now_at(crate::now_ms()) }
    /// Same as `now` with an explicit wall clock (for tests).
    pub fn now_at(&self, wall_ms: u64) -> u64 {
        let wall = wall_ms.saturating_mul(1 << LOGICAL_BITS);
        let mut last = self.last.lock();
        let next = if wall > *last { wall } else { last.saturating_add(1) };
        *last = next;
        next
    }
    /// Fold in a timestamp received from a peer (ignored if it is implausibly far ahead).
    pub fn observe(&self, remote: u64) { self.observe_at(remote, crate::now_ms()) }
    /// Same as `observe` with an explicit wall clock (for tests).
    pub fn observe_at(&self, remote: u64, wall_ms: u64) {
        if hlc_ms(remote) > wall_ms.saturating_add(crate::state::MAX_FUTURE_MS) {
            return;
        }
        let mut last = self.last.lock();
        if remote > *last {
            *last = remote;
        }
    }
    /// The last value handed out or observed.
    pub fn last(&self) -> u64 { *self.last.lock() }
}

/// Milliseconds part of an hlc value.
pub fn hlc_ms(hlc: u64) -> u64 { hlc >> LOGICAL_BITS }

/// The smallest hlc value with the given milliseconds part (logical counter 0).
pub fn hlc_from_ms(ms: u64) -> u64 { ms.saturating_mul(1 << LOGICAL_BITS) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strictly_increasing_even_when_wall_goes_back() {
        let c = Hlc::new();
        let a = c.now_at(1_000);
        let b = c.now_at(1_000);
        let d = c.now_at(500);
        let e = c.now_at(2_000);
        assert!(a < b && b < d && d < e);
        assert_eq!(hlc_ms(a), 1_000);
        assert_eq!(hlc_ms(d), 1_000);
        assert_eq!(hlc_ms(e), 2_000);
        assert_eq!(a, hlc_from_ms(1_000));
    }

    #[test]
    fn observe_moves_forward_only() {
        let c = Hlc::with_floor(hlc_from_ms(1_000));
        c.observe_at(hlc_from_ms(1_500) + 3, 1_200);
        assert_eq!(c.last(), hlc_from_ms(1_500) + 3);
        let next = c.now_at(1_200);
        assert_eq!(next, hlc_from_ms(1_500) + 4);
        c.observe_at(hlc_from_ms(10), 1_200);
        assert_eq!(c.last(), next);
    }

    #[test]
    fn observe_ignores_far_future() {
        let c = Hlc::with_floor(hlc_from_ms(1_000));
        c.observe_at(u64::MAX, 1_000);
        assert_eq!(c.last(), hlc_from_ms(1_000));
        c.observe_at(hlc_from_ms(1_000 + crate::state::MAX_FUTURE_MS + 1), 1_000);
        assert_eq!(c.last(), hlc_from_ms(1_000));
        c.observe_at(hlc_from_ms(1_000 + crate::state::MAX_FUTURE_MS), 1_000);
        assert_eq!(c.last(), hlc_from_ms(1_000 + crate::state::MAX_FUTURE_MS));
    }

    #[test]
    fn saturates_instead_of_overflowing() {
        let c = Hlc::with_floor(u64::MAX);
        assert_eq!(c.now_at(5), u64::MAX);
        assert_eq!(hlc_from_ms(u64::MAX), u64::MAX);
    }

    #[test]
    fn real_clock_is_recent() {
        let c = Hlc::new();
        let t = c.now();
        assert!(hlc_ms(t) > 1_704_067_200_000);
    }
}
