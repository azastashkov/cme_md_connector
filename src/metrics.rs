//! Per-stage latency measurement and the built-in dashboard.
//!
//! The hot thread records nanosecond stage deltas into HdrHistograms (the
//! end-to-end "total" stages use `record_correct` to defend against coordinated
//! omission under open-loop load). A reporter thread periodically swaps out the
//! interval histograms, merges them into a cumulative set, computes
//! p50/p95/p99, and publishes an immutable [`MetricsSnapshot`] that the
//! dashboard serves.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hdrhistogram::Histogram;

pub mod dashboard;

/// The measured stages, in dashboard display order.
pub const STAGE_NAMES: [&str; 7] = [
    "decode",
    "book",
    "signal",
    "risk",
    "gateway",
    "tick_to_signal",
    "tick_to_order",
];

const HIST_LOW: u64 = 1;
const HIST_HIGH: u64 = 60_000_000_000; // 60s
const HIST_SIGFIG: u8 = 3;

#[inline]
fn clamp(v: u64) -> u64 {
    v.clamp(HIST_LOW, HIST_HIGH)
}

/// One tick's stage timings. The `risk`/`gateway`/`tick_to_order` fields are
/// present only on ticks that produced an order candidate / a sent order.
#[derive(Debug, Default, Clone, Copy)]
pub struct StageSample {
    pub decode_ns: u64,
    pub book_ns: u64,
    pub signal_ns: u64,
    pub tick_to_signal_ns: u64,
    pub risk_ns: Option<u64>,
    pub gateway_ns: Option<u64>,
    pub tick_to_order_ns: Option<u64>,
}

/// Percentile summary for one stage over a window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageStat {
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
    pub count: u64,
}

fn stat(h: &Histogram<u64>) -> StageStat {
    StageStat {
        p50: h.value_at_quantile(0.50),
        p95: h.value_at_quantile(0.95),
        p99: h.value_at_quantile(0.99),
        max: h.max(),
        count: h.len(),
    }
}

struct Histos {
    decode: Histogram<u64>,
    book: Histogram<u64>,
    signal: Histogram<u64>,
    risk: Histogram<u64>,
    gateway: Histogram<u64>,
    tick_to_signal: Histogram<u64>,
    tick_to_order: Histogram<u64>,
}

impl Histos {
    fn new() -> Histos {
        let h = || Histogram::new_with_bounds(HIST_LOW, HIST_HIGH, HIST_SIGFIG).unwrap();
        Histos {
            decode: h(),
            book: h(),
            signal: h(),
            risk: h(),
            gateway: h(),
            tick_to_signal: h(),
            tick_to_order: h(),
        }
    }

    fn record(&mut self, s: &StageSample, expected_interval_ns: u64) {
        let ei = expected_interval_ns.max(1);
        let _ = self.decode.record(clamp(s.decode_ns));
        let _ = self.book.record(clamp(s.book_ns));
        let _ = self.signal.record(clamp(s.signal_ns));
        // End-to-end stages correct for coordinated omission.
        let _ = self
            .tick_to_signal
            .record_correct(clamp(s.tick_to_signal_ns), ei);
        if let Some(v) = s.risk_ns {
            let _ = self.risk.record(clamp(v));
        }
        if let Some(v) = s.gateway_ns {
            let _ = self.gateway.record(clamp(v));
        }
        if let Some(v) = s.tick_to_order_ns {
            let _ = self.tick_to_order.record_correct(clamp(v), ei);
        }
    }

    fn add(&mut self, other: &Histos) {
        let _ = self.decode.add(&other.decode);
        let _ = self.book.add(&other.book);
        let _ = self.signal.add(&other.signal);
        let _ = self.risk.add(&other.risk);
        let _ = self.gateway.add(&other.gateway);
        let _ = self.tick_to_signal.add(&other.tick_to_signal);
        let _ = self.tick_to_order.add(&other.tick_to_order);
    }

    fn stats(&self) -> Vec<(&'static str, StageStat)> {
        vec![
            (STAGE_NAMES[0], stat(&self.decode)),
            (STAGE_NAMES[1], stat(&self.book)),
            (STAGE_NAMES[2], stat(&self.signal)),
            (STAGE_NAMES[3], stat(&self.risk)),
            (STAGE_NAMES[4], stat(&self.gateway)),
            (STAGE_NAMES[5], stat(&self.tick_to_signal)),
            (STAGE_NAMES[6], stat(&self.tick_to_order)),
        ]
    }
}

#[derive(Default)]
struct Counters {
    ticks: AtomicU64,
    orders: AtomicU64,
    rejects: AtomicU64,
    drops: AtomicU64,
    /// Latest aggregate signed net position (a gauge, last-write-wins).
    position: AtomicI64,
}

/// Hot-path handle for recording stage timings. Cheap to clone (Arc-backed).
#[derive(Clone)]
pub struct MetricsSink {
    histos: Arc<Mutex<Histos>>,
    counters: Arc<Counters>,
    expected_interval_ns: u64,
}

impl MetricsSink {
    /// Record one tick's stage timings.
    #[inline]
    pub fn record(&self, s: &StageSample) {
        self.counters.ticks.fetch_add(1, Ordering::Relaxed);
        let mut g = self.histos.lock().unwrap();
        g.record(s, self.expected_interval_ns);
    }

    #[inline]
    pub fn inc_order(&self) {
        self.counters.orders.fetch_add(1, Ordering::Relaxed);
    }
    /// Publish the latest aggregate signed net position (a gauge).
    #[inline]
    pub fn set_position(&self, net: i64) {
        self.counters.position.store(net, Ordering::Relaxed);
    }
    #[inline]
    pub fn inc_reject(&self) {
        self.counters.rejects.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn inc_drop(&self) {
        self.counters.drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// An immutable snapshot published to the dashboard each interval.
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    pub interval: Vec<(&'static str, StageStat)>,
    pub cumulative: Vec<(&'static str, StageStat)>,
    pub throughput_pps: f64,
    pub ticks: u64,
    pub orders: u64,
    pub rejects: u64,
    pub drops: u64,
    pub timer_resolution_ns: u64,
    pub total_pnl: f64,
    /// Aggregate signed net position across all instruments.
    pub net_position: i64,
    /// Drawdown from the running PnL peak, as a percent of that peak (<= 0).
    pub drawdown_pct: f64,
}

/// Off-hot-path aggregator: swaps interval histograms, accumulates a cumulative
/// set, and builds snapshots.
pub struct Reporter {
    histos: Arc<Mutex<Histos>>,
    counters: Arc<Counters>,
    cumulative: Histos,
    timer_resolution_ns: u64,
    last_ticks: u64,
    last_instant: Instant,
    /// Running high-water mark of total PnL, for drawdown.
    peak_pnl: f64,
}

impl Reporter {
    /// Build a snapshot of the most recent interval and update cumulative stats.
    pub fn snapshot(&mut self, total_pnl: f64) -> MetricsSnapshot {
        let interval = {
            let mut g = self.histos.lock().unwrap();
            std::mem::replace(&mut *g, Histos::new())
        };
        self.cumulative.add(&interval);

        let ticks = self.counters.ticks.load(Ordering::Relaxed);
        let now = Instant::now();
        let dt = now.duration_since(self.last_instant).as_secs_f64();
        let throughput_pps = if dt > 0.0 {
            (ticks.saturating_sub(self.last_ticks)) as f64 / dt
        } else {
            0.0
        };
        self.last_ticks = ticks;
        self.last_instant = now;

        // Drawdown from the high-water mark, as a percent of the peak. Only
        // meaningful once PnL has been positive; <= 0 since total_pnl <= peak.
        self.peak_pnl = self.peak_pnl.max(total_pnl);
        let drawdown_pct = if self.peak_pnl > 0.0 {
            (total_pnl - self.peak_pnl) / self.peak_pnl * 100.0
        } else {
            0.0
        };

        MetricsSnapshot {
            interval: interval.stats(),
            cumulative: self.cumulative.stats(),
            throughput_pps,
            ticks,
            orders: self.counters.orders.load(Ordering::Relaxed),
            rejects: self.counters.rejects.load(Ordering::Relaxed),
            drops: self.counters.drops.load(Ordering::Relaxed),
            timer_resolution_ns: self.timer_resolution_ns,
            total_pnl,
            net_position: self.counters.position.load(Ordering::Relaxed),
            drawdown_pct,
        }
    }

    /// The cumulative per-stage stats over the whole run (for a final report).
    pub fn cumulative_stats(&self) -> Vec<(&'static str, StageStat)> {
        self.cumulative.stats()
    }
}

/// Create a linked `(MetricsSink, Reporter)` pair.
///
/// `expected_interval_ns` is the open-loop inter-arrival time (1e9 / rate) used
/// for coordinated-omission correction on the end-to-end stages.
pub fn new_metrics(expected_interval_ns: u64, timer_resolution_ns: u64) -> (MetricsSink, Reporter) {
    let histos = Arc::new(Mutex::new(Histos::new()));
    let counters = Arc::new(Counters::default());
    let sink = MetricsSink {
        histos: Arc::clone(&histos),
        counters: Arc::clone(&counters),
        expected_interval_ns,
    };
    let reporter = Reporter {
        histos,
        counters,
        cumulative: Histos::new(),
        timer_resolution_ns,
        last_ticks: 0,
        last_instant: Instant::now(),
        peak_pnl: f64::NEG_INFINITY,
    };
    (sink, reporter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_percentiles_for_a_stage() {
        let (sink, mut rep) = new_metrics(1000, 42);
        for _ in 0..1000 {
            sink.record(&StageSample {
                decode_ns: 100,
                ..Default::default()
            });
        }
        let snap = rep.snapshot(0.0);
        let decode = snap.interval.iter().find(|(n, _)| *n == "decode").unwrap().1;
        assert_eq!(decode.count, 1000);
        assert!((99..=101).contains(&decode.p50), "p50 was {}", decode.p50);
        assert!((99..=101).contains(&decode.p99), "p99 was {}", decode.p99);
    }

    #[test]
    fn interval_resets_but_cumulative_accumulates() {
        let (sink, mut rep) = new_metrics(1000, 42);
        for _ in 0..10 {
            sink.record(&StageSample {
                decode_ns: 50,
                ..Default::default()
            });
        }
        let first = rep.snapshot(0.0);
        assert_eq!(first.interval[0].1.count, 10);

        // No new records -> interval empty, cumulative retains the 10.
        let second = rep.snapshot(0.0);
        assert_eq!(second.interval[0].1.count, 0);
        assert_eq!(second.cumulative[0].1.count, 10);
    }

    #[test]
    fn counters_track_orders_rejects_and_drops() {
        let (sink, mut rep) = new_metrics(1000, 42);
        sink.inc_order();
        sink.inc_order();
        sink.inc_reject();
        sink.inc_drop();
        sink.set_position(-3);
        sink.record(&StageSample::default());
        let snap = rep.snapshot(0.0);
        assert_eq!(snap.orders, 2);
        assert_eq!(snap.rejects, 1);
        assert_eq!(snap.drops, 1);
        assert_eq!(snap.ticks, 1);
        assert_eq!(snap.net_position, -3);
    }

    #[test]
    fn drawdown_tracks_decline_from_the_running_peak() {
        let (_sink, mut rep) = new_metrics(1000, 42);
        // Non-positive peak so far -> drawdown is defined as 0.
        assert_eq!(rep.snapshot(-5.0).drawdown_pct, 0.0);
        // New high-water mark -> at peak, no drawdown.
        assert_eq!(rep.snapshot(100.0).drawdown_pct, 0.0);
        // Falls to 80 against a peak of 100 -> (80-100)/100*100 = -20%.
        assert!((rep.snapshot(80.0).drawdown_pct - (-20.0)).abs() < 1e-9);
        // Recovering to a new high resets the drawdown to 0.
        assert_eq!(rep.snapshot(120.0).drawdown_pct, 0.0);
    }

    #[test]
    fn coordinated_omission_correction_backfills_the_tail() {
        // Expected interval 100ns, but one sample stalls for 10_000ns: record_correct
        // synthesizes the omitted samples, so the recorded count exceeds 1.
        let (sink, mut rep) = new_metrics(100, 42);
        sink.record(&StageSample {
            tick_to_signal_ns: 10_000,
            ..Default::default()
        });
        let snap = rep.snapshot(0.0);
        let t2s = snap
            .interval
            .iter()
            .find(|(n, _)| *n == "tick_to_signal")
            .unwrap()
            .1;
        assert!(t2s.count > 1, "expected backfilled samples, got {}", t2s.count);
    }
}
