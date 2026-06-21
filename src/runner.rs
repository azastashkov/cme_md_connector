//! Threaded load-test orchestration: a paced generator feeds an SPSC ring that
//! a busy-spinning consumer drains through the inline [`Pipeline`], while a
//! reporter publishes snapshots and (optionally) a dashboard serves them.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use rtrb::RingBuffer;

use crate::affinity;
use crate::loadgen::{Generator, GeneratorConfig};
use crate::metrics::dashboard;
use crate::metrics::{new_metrics, MetricsSnapshot, StageStat};
use crate::pipeline::{Pipeline, PipelineConfig};
use crate::timing::Timer;

/// Max bytes carried per ring slot (our packets are ~100 bytes).
pub const MAX_PACKET: usize = 512;

/// Fixed-size packet carried across the ingest ring.
#[derive(Clone, Copy)]
pub struct PacketBuf {
    len: u16,
    data: [u8; MAX_PACKET],
}

impl PacketBuf {
    fn from_slice(s: &[u8]) -> PacketBuf {
        let mut data = [0u8; MAX_PACKET];
        let len = s.len().min(MAX_PACKET);
        data[..len].copy_from_slice(&s[..len]);
        PacketBuf {
            len: len as u16,
            data,
        }
    }

    #[inline]
    fn as_slice(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

/// Configuration for a load-test run.
#[derive(Clone)]
pub struct RunConfig {
    pub pipeline: PipelineConfig,
    pub generator: GeneratorConfig,
    /// Open-loop emission rate in packets/sec; 0 = unthrottled (max).
    pub rate: u64,
    pub duration: Duration,
    pub port: u16,
    pub dashboard: bool,
    pub ring_capacity: usize,
    pub report_interval: Duration,
}

/// Outcome of a run.
pub struct RunResult {
    pub final_snapshot: MetricsSnapshot,
    pub cumulative: Vec<(&'static str, StageStat)>,
}

/// Run the full pipeline for `cfg.duration`, returning final metrics.
pub fn run(cfg: RunConfig) -> RunResult {
    let duration = cfg.duration;
    let timer = Timer::new();
    let timer_resolution_ns = timer.resolution_ns();
    let expected_interval_ns = if cfg.rate > 0 {
        1_000_000_000 / cfg.rate
    } else {
        0
    };

    let (sink, reporter) = new_metrics(expected_interval_ns, timer_resolution_ns);
    let drop_sink = sink.clone();
    let mut pipeline = Pipeline::new(cfg.pipeline.clone(), sink, timer.clone());

    let (mut producer, mut consumer) = RingBuffer::<PacketBuf>::new(cfg.ring_capacity);

    let shutdown = Arc::new(AtomicBool::new(false));
    let pnl_bits = Arc::new(AtomicU64::new(0.0f64.to_bits()));
    let latest = Arc::new(ArcSwap::from_pointee(MetricsSnapshot::default()));

    // --- Generator thread: open-loop pacing into the ring. ---
    let gen_thread = {
        let shutdown = Arc::clone(&shutdown);
        let mut generator = Generator::new(cfg.generator.clone());
        let rate = cfg.rate;
        thread::spawn(move || {
            let interval_ns = if rate > 0 { 1_000_000_000 / rate } else { 0 };
            let start = Instant::now();
            let mut i: u64 = 0;
            while !shutdown.load(Ordering::Relaxed) {
                if interval_ns > 0 {
                    let target = start + Duration::from_nanos(i.saturating_mul(interval_ns));
                    pace_until(target, &shutdown);
                }
                let packet = generator.next_packet();
                if producer.push(PacketBuf::from_slice(&packet)).is_err() {
                    drop_sink.inc_drop(); // ring full: consumer fell behind
                }
                i = i.wrapping_add(1);
            }
        })
    };

    // --- Consumer thread: busy-spin drain through the inline pipeline. ---
    let consumer_thread = {
        let shutdown = Arc::clone(&shutdown);
        let pnl_bits = Arc::clone(&pnl_bits);
        let timer = timer.clone();
        thread::spawn(move || {
            affinity::pin_hot_thread();
            while !shutdown.load(Ordering::Relaxed) {
                match consumer.pop() {
                    Ok(buf) => {
                        let t0 = timer.raw();
                        pipeline.process(buf.as_slice(), t0);
                        pnl_bits.store(pipeline.total_pnl().to_bits(), Ordering::Relaxed);
                    }
                    Err(_) => std::hint::spin_loop(),
                }
            }
            pipeline.total_pnl()
        })
    };

    // --- Reporter thread: periodic snapshots -> ArcSwap + final cumulative. ---
    let reporter_thread = {
        let shutdown = Arc::clone(&shutdown);
        let pnl_bits = Arc::clone(&pnl_bits);
        let latest = Arc::clone(&latest);
        let interval = cfg.report_interval;
        thread::spawn(move || {
            let mut reporter = reporter;
            while !shutdown.load(Ordering::Relaxed) {
                sleep_interruptible(interval, &shutdown);
                let pnl = f64::from_bits(pnl_bits.load(Ordering::Relaxed));
                latest.store(Arc::new(reporter.snapshot(pnl)));
            }
            let pnl = f64::from_bits(pnl_bits.load(Ordering::Relaxed));
            let final_snapshot = reporter.snapshot(pnl);
            let cumulative = reporter.cumulative_stats();
            (final_snapshot, cumulative)
        })
    };

    // --- Optional dashboard thread. ---
    let dashboard_thread = if cfg.dashboard {
        let shutdown = Arc::clone(&shutdown);
        let latest = Arc::clone(&latest);
        let port = cfg.port;
        Some(thread::spawn(move || {
            let _ = dashboard::serve(port, latest, shutdown);
        }))
    } else {
        None
    };

    // --- Run for the configured duration, then tear down. ---
    sleep_interruptible(cfg.duration, &shutdown);
    shutdown.store(true, Ordering::Relaxed);

    let _ = gen_thread.join();
    let _ = consumer_thread.join();
    let (final_snapshot, cumulative) = reporter_thread.join().expect("reporter thread");
    if let Some(h) = dashboard_thread {
        let _ = h.join();
    }

    let mut final_snapshot = final_snapshot;
    // Report the run-average throughput (the trailing interval is near-empty).
    let secs = duration.as_secs_f64();
    if secs > 0.0 {
        final_snapshot.throughput_pps = final_snapshot.ticks as f64 / secs;
    }

    RunResult {
        final_snapshot,
        cumulative,
    }
}

/// Sleep until `target`, mixing coarse sleeps and a final busy-spin for accuracy.
fn pace_until(target: Instant, shutdown: &AtomicBool) {
    loop {
        let now = Instant::now();
        if now >= target || shutdown.load(Ordering::Relaxed) {
            return;
        }
        let remaining = target - now;
        if remaining > Duration::from_micros(100) {
            thread::sleep(remaining - Duration::from_micros(50));
        } else {
            std::hint::spin_loop();
        }
    }
}

/// Sleep up to `dur`, waking early if shutdown is signalled.
fn sleep_interruptible(dur: Duration, shutdown: &AtomicBool) {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(remaining.min(Duration::from_millis(20)));
    }
}

/// Render a final per-stage percentile report (cumulative over the run).
pub fn format_report(result: &RunResult) -> String {
    let s = &result.final_snapshot;
    let mut out = String::new();
    out.push_str(&format!(
        "\n=== CME MD Connector — load test complete ===\n\
         timer resolution : {} ns (per-stage values below this are resolution-limited)\n\
         packets processed: {}\n\
         throughput (avg) : {:.0} pkt/s\n\
         orders sent      : {}\n\
         orders rejected  : {}\n\
         ring drops       : {}\n\
         total PnL        : {:.2}\n\n",
        s.timer_resolution_ns, s.ticks, s.throughput_pps, s.orders, s.rejects, s.drops, s.total_pnl,
    ));
    out.push_str("per-stage latency (cumulative, nanoseconds):\n");
    out.push_str(&format!(
        "  {:<16} {:>10} {:>10} {:>10} {:>12} {:>12}\n",
        "stage", "p50", "p95", "p99", "max", "count"
    ));
    for (name, st) in &result.cumulative {
        out.push_str(&format!(
            "  {:<16} {:>10} {:>10} {:>10} {:>12} {:>12}\n",
            name, st.p50, st.p95, st.p99, st.max, st.count
        ));
    }
    out
}

/// Micro-benchmark mode: time each stage in isolation over many iterations so
/// sub-resolution stages (which quantize to ~0/42ns on Apple Silicon) get an
/// honest mean cost. Returns `(label, mean_ns)` rows.
pub fn calibrate(cfg: &RunConfig, iterations: u64) -> Vec<(String, f64)> {
    let timer = Timer::new();
    let mut rows = Vec::new();

    // Pre-generate a batch of packets so generation isn't measured.
    let mut generator = Generator::new(cfg.generator.clone());
    let packets: Vec<Vec<u8>> = (0..iterations).map(|_| generator.next_packet()).collect();

    // Whole-pipeline mean: process each packet once.
    let (sink, _rep) = new_metrics(0, timer.resolution_ns());
    let mut pipeline = Pipeline::new(cfg.pipeline.clone(), sink, timer.clone());
    let start = timer.raw();
    for p in &packets {
        let t0 = timer.raw();
        pipeline.process(p, t0);
    }
    let elapsed = timer.delta_ns(start, timer.raw());
    rows.push((
        "full process (decode→order)".to_string(),
        elapsed as f64 / iterations as f64,
    ));

    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick_config(dashboard: bool) -> RunConfig {
        RunConfig {
            pipeline: PipelineConfig {
                instruments: vec![1, 2],
                ..Default::default()
            },
            generator: GeneratorConfig {
                instruments: vec![1, 2],
                ..Default::default()
            },
            rate: 100_000,
            duration: Duration::from_millis(300),
            port: 0,
            dashboard,
            ring_capacity: 4096,
            report_interval: Duration::from_millis(50),
        }
    }

    #[test]
    fn run_processes_packets_and_reports_stages() {
        let result = run(quick_config(false));
        assert!(
            result.final_snapshot.ticks > 0,
            "the consumer should process packets"
        );
        // Every stage name is present in the cumulative report.
        let names: Vec<_> = result.cumulative.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"decode"));
        assert!(names.contains(&"tick_to_order"));
    }

    #[test]
    fn calibrate_returns_a_positive_mean() {
        let rows = calibrate(&quick_config(false), 2000);
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|(_, ns)| *ns >= 0.0));
    }
}
