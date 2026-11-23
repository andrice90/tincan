//! Global upload token bucket, driven by iroh-blobs `Throttle` provider events.
//!
//! The provider asks before sending each chunk (usually 16 KiB). The bucket answers with
//! how long that chunk has to wait so that all uploads together stay under the limit.
//! Waits are reserved up front (the bucket may go into debt), so concurrent uploads queue
//! behind each other instead of all firing when tokens refill.

use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Tokens the bucket may save up: about one chunk, so a limit takes effect at once.
const BURST_BYTES: f64 = 16.0 * 1024.0;
/// Lowest accepted limit; `Some(0)` would stall uploads forever.
pub(crate) const MIN_LIMIT: u64 = 1024;

#[derive(Debug)]
pub(crate) struct UploadLimiter {
    inner: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    /// Bytes per second, `None` = unlimited.
    rate: Option<u64>,
    /// Available bytes; negative while chunks are scheduled ahead.
    tokens: f64,
    last: Instant,
}

impl UploadLimiter {
    pub(crate) fn new() -> Self {
        Self { inner: Mutex::new(Bucket { rate: None, tokens: BURST_BYTES, last: Instant::now() }) }
    }

    /// Sets the limit in bytes per second (`None` = unlimited). Values below
    /// [`MIN_LIMIT`] are raised to it.
    pub(crate) fn set_limit(&self, bytes_per_sec: Option<u64>) {
        let mut b = self.inner.lock();
        b.rate = bytes_per_sec.map(|r| r.max(MIN_LIMIT));
        b.tokens = BURST_BYTES;
        b.last = Instant::now();
    }

    /// The current limit.
    pub(crate) fn limit(&self) -> Option<u64> {
        self.inner.lock().rate
    }

    /// Reserves `bytes` and returns how long the caller must wait before sending them.
    pub(crate) fn reserve(&self, bytes: u64) -> Duration {
        self.reserve_at(bytes, Instant::now())
    }

    fn reserve_at(&self, bytes: u64, now: Instant) -> Duration {
        let mut b = self.inner.lock();
        let Some(rate) = b.rate else {
            return Duration::ZERO;
        };
        let rate = rate as f64;
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * rate).min(BURST_BYTES);
        b.last = now;
        b.tokens -= bytes as f64;
        if b.tokens >= 0.0 { Duration::ZERO } else { Duration::from_secs_f64(-b.tokens / rate) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_waits() {
        let l = UploadLimiter::new();
        for _ in 0..1000 {
            assert_eq!(l.reserve(16 * 1024), Duration::ZERO);
        }
    }

    #[test]
    fn limited_paces_to_rate() {
        let l = UploadLimiter::new();
        l.set_limit(Some(100 * 1024));
        let t0 = Instant::now();
        // 1 MiB in 16 KiB chunks at 100 KiB/s: the last chunk waits about 10 s minus the burst.
        let mut last = Duration::ZERO;
        for _ in 0..64 {
            last = l.reserve_at(16 * 1024, t0);
        }
        let expected = (1024.0 * 1024.0 - BURST_BYTES) / (100.0 * 1024.0);
        assert!((last.as_secs_f64() - expected).abs() < 0.01, "{last:?} vs {expected}");
        // Time passing pays the debt back.
        let later = t0 + Duration::from_secs(20);
        assert_eq!(l.reserve_at(1024, later), Duration::ZERO);
    }

    #[test]
    fn zero_limit_is_clamped() {
        let l = UploadLimiter::new();
        l.set_limit(Some(0));
        assert_eq!(l.limit(), Some(MIN_LIMIT));
        l.set_limit(None);
        assert_eq!(l.limit(), None);
    }
}
