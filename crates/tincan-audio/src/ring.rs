//! Wait-free single-producer single-consumer sample ring shared between a cpal callback and the
//! engine thread. Samples are stored as `f32` bits in `AtomicU32`s, so the ring is entirely safe
//! Rust, never allocates after construction and never blocks.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Lock-free SPSC ring of `f32` samples. Exactly one thread may push and one thread may pop.
pub(crate) struct SampleRing {
    buf: Box<[AtomicU32]>,
    mask: usize,
    /// Total samples ever written (wrapping).
    head: AtomicUsize,
    /// Total samples ever read (wrapping).
    tail: AtomicUsize,
}

impl SampleRing {
    /// Creates a ring holding at least `min_capacity` samples (rounded up to a power of two).
    pub(crate) fn new(min_capacity: usize) -> Self {
        let cap = min_capacity.max(2).next_power_of_two();
        let buf = (0..cap).map(|_| AtomicU32::new(0)).collect::<Vec<_>>().into_boxed_slice();
        Self { buf, mask: cap - 1, head: AtomicUsize::new(0), tail: AtomicUsize::new(0) }
    }

    /// Capacity in samples.
    pub(crate) fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// Samples currently buffered.
    pub(crate) fn len(&self) -> usize {
        let h = self.head.load(Ordering::Acquire);
        let t = self.tail.load(Ordering::Acquire);
        h.wrapping_sub(t)
    }

    /// Producer: writes samples from `iter` until the ring is full. Returns how many were written.
    pub(crate) fn push_iter(&self, iter: impl Iterator<Item = f32>) -> usize {
        let h = self.head.load(Ordering::Relaxed);
        let t = self.tail.load(Ordering::Acquire);
        let free = self.capacity() - h.wrapping_sub(t);
        let mut n = 0;
        for s in iter.take(free) {
            self.buf[h.wrapping_add(n) & self.mask].store(s.to_bits(), Ordering::Relaxed);
            n += 1;
        }
        self.head.store(h.wrapping_add(n), Ordering::Release);
        n
    }

    /// Producer: writes as much of `src` as fits. Returns how many samples were written.
    pub(crate) fn push_slice(&self, src: &[f32]) -> usize {
        self.push_iter(src.iter().copied())
    }

    /// Consumer: reads up to `dst.len()` samples. Returns how many were read.
    pub(crate) fn pop_slice(&self, dst: &mut [f32]) -> usize {
        let t = self.tail.load(Ordering::Relaxed);
        let h = self.head.load(Ordering::Acquire);
        let n = h.wrapping_sub(t).min(dst.len());
        for (i, d) in dst.iter_mut().take(n).enumerate() {
            *d = f32::from_bits(self.buf[t.wrapping_add(i) & self.mask].load(Ordering::Relaxed));
        }
        self.tail.store(t.wrapping_add(n), Ordering::Release);
        n
    }

    /// Consumer: reads one sample.
    #[cfg(test)]
    pub(crate) fn pop(&self) -> Option<f32> {
        let mut one = [0f32];
        (self.pop_slice(&mut one) == 1).then_some(one[0])
    }

    /// Consumer: discards up to `n` samples. Returns how many were discarded.
    pub(crate) fn skip(&self, n: usize) -> usize {
        let t = self.tail.load(Ordering::Relaxed);
        let h = self.head.load(Ordering::Acquire);
        let k = h.wrapping_sub(t).min(n);
        self.tail.store(t.wrapping_add(k), Ordering::Release);
        k
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn push_pop_wraps() {
        let r = SampleRing::new(5);
        assert_eq!(r.capacity(), 8);
        assert_eq!(r.push_slice(&[1.0, 2.0, 3.0]), 3);
        let mut out = [0f32; 2];
        assert_eq!(r.pop_slice(&mut out), 2);
        assert_eq!(out, [1.0, 2.0]);
        assert_eq!(r.push_slice(&[4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0]), 7);
        assert_eq!(r.len(), 8);
        assert_eq!(r.skip(1), 1);
        let mut all = [0f32; 16];
        assert_eq!(r.pop_slice(&mut all), 7);
        // 11.0 did not fit; 3.0 was skipped
        assert_eq!(&all[..7], &[4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]);
        assert_eq!(r.pop(), None);
    }

    #[test]
    fn concurrent_spsc_keeps_order() {
        let r = Arc::new(SampleRing::new(64));
        let p = r.clone();
        let producer = std::thread::spawn(move || {
            let mut next = 0u32;
            while next < 20_000 {
                let n = p.push_iter((next..20_000).map(|v| v as f32));
                next += n as u32;
                if n == 0 {
                    std::thread::yield_now();
                }
            }
        });
        let mut expect = 0u32;
        let mut buf = [0f32; 17];
        while expect < 20_000 {
            let n = r.pop_slice(&mut buf);
            for v in &buf[..n] {
                assert_eq!(*v, expect as f32);
                expect += 1;
            }
            if n == 0 {
                std::thread::yield_now();
            }
        }
        producer.join().unwrap();
    }
}
