//! Low-overhead per-stage timestamps.
//!
//! The portable default uses [`quanta`], which is TSC-backed on Linux/Windows
//! x86 and falls back to the OS monotonic clock elsewhere. On Apple Silicon
//! there is no userspace cycle counter, so the effective resolution is the
//! ~41.67 ns ARM generic timer — [`Timer::resolution_ns`] measures and surfaces
//! that floor so the dashboard can label sub-resolution stages honestly.
//!
//! A true sub-10 ns per-stage path (rdtscp / `minstant`) belongs behind the
//! `linux-x86-fastpath` feature on Linux x86_64; the portable `Timer` below is
//! the universal backend.

use quanta::Clock;

/// A monotonic clock used to stamp pipeline stage boundaries.
#[derive(Clone)]
pub struct Timer {
    clock: Clock,
}

impl Default for Timer {
    fn default() -> Timer {
        Timer::new()
    }
}

impl Timer {
    pub fn new() -> Timer {
        Timer {
            clock: Clock::new(),
        }
    }

    /// Read the raw counter (cheapest read). Stamp stage boundaries with this
    /// and convert deltas to ns off the hot path with [`Timer::delta_ns`].
    #[inline(always)]
    pub fn raw(&self) -> u64 {
        self.clock.raw()
    }

    /// Convert a raw `[start, end]` interval to nanoseconds.
    #[inline]
    pub fn delta_ns(&self, start: u64, end: u64) -> u64 {
        if end <= start {
            return 0;
        }
        self.clock.delta(start, end).as_nanos() as u64
    }

    /// Measure the effective timer resolution (smallest non-zero delta) in ns.
    /// On Apple Silicon this is ~42 ns; on x86 with a TSC it is a few ns.
    pub fn resolution_ns(&self) -> u64 {
        let mut min = u64::MAX;
        let mut prev = self.clock.raw();
        // Span well past the timer period so multiple ticks are observed.
        for _ in 0..200_000 {
            let now = self.clock.raw();
            let d = self.delta_ns(prev, now);
            if d > 0 {
                min = min.min(d);
            }
            prev = now;
        }
        if min == u64::MAX {
            1
        } else {
            min
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_is_monotonic_non_decreasing() {
        let t = Timer::new();
        let a = t.raw();
        let b = t.raw();
        assert!(b >= a);
    }

    #[test]
    fn delta_of_equal_raw_is_zero() {
        let t = Timer::new();
        let r = t.raw();
        assert_eq!(t.delta_ns(r, r), 0);
        // An inverted interval is clamped to zero rather than underflowing.
        assert_eq!(t.delta_ns(r + 5, r), 0);
    }

    #[test]
    fn delta_of_a_real_interval_is_positive() {
        let t = Timer::new();
        let start = t.raw();
        // Spin until the monotonic clock advances past its own resolution.
        let mut end = t.raw();
        while t.delta_ns(start, end) == 0 {
            end = t.raw();
        }
        assert!(t.delta_ns(start, end) > 0);
    }

    #[test]
    fn resolution_is_positive_and_plausible() {
        let t = Timer::new();
        let res = t.resolution_ns();
        assert!(res >= 1, "resolution must be positive");
        // Sanity bound: any reasonable monotonic clock resolves below 1ms.
        assert!(res < 1_000_000, "resolution {res}ns is implausibly coarse");
    }
}
