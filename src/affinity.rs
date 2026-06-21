//! Hot-thread placement.
//!
//! Hard thread-to-core pinning is unavailable on macOS / Apple Silicon
//! (`core_affinity::set_for_current` is a no-op there — XNU exposes no affinity
//! sets on ARM), so the portable default does nothing. Real `sched_setaffinity`
//! pinning belongs behind the `pin-threads` feature on Linux; QoS biasing toward
//! performance cores is the only lever on macOS and is left as a documented
//! extension point. See the README "Platform caveats".

/// Best-effort placement of the latency-critical consumer thread.
#[inline]
pub fn pin_hot_thread() {
    #[cfg(all(feature = "pin-threads", target_os = "linux", target_arch = "x86_64"))]
    {
        // Linux fast path (enable the `pin-threads` feature and add the
        // `core_affinity` crate): pin to a dedicated, isolated core.
        // core_affinity::set_for_current(core_affinity::CoreId { id: LAST_CORE });
    }
    // Default / macOS: no-op (no hard affinity available).
}
