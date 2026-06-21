//! The inline, single-threaded hot path: decode -> book -> signal -> risk ->
//! gateway, with every stage boundary timestamped and recorded.
//!
//! `process` is called once per packet by the consumer thread; `t0` is stamped
//! by the caller at the moment the packet is taken from the ingest ring (so the
//! ring hand-off is excluded from the measured latency).

use arrayvec::ArrayVec;

use crate::gateway::MockGateway;
use crate::mdp3::book_refresh::{IncrementalRefresh, MdEntry};
use crate::mdp3::header::messages;
use crate::mdp3::packet::PacketHeader;
use crate::metrics::{MetricsSink, StageSample};
use crate::orderbook::{Bbo, OrderBook};
use crate::risk::{OrderRequest, RiskConfig, RiskManager};
use crate::signal::{SignalConfig, SignalEngine};
use crate::timing::Timer;
use crate::{InstrumentId, OrderSide};

/// Max entries decoded from a single packet before overflow is dropped.
const MAX_ENTRIES_PER_PACKET: usize = 64;

/// Configuration for the whole pipeline.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub instruments: Vec<InstrumentId>,
    pub multiplier: f64,
    pub order_qty: i64,
    pub signal: SignalConfig,
    pub risk: RiskConfig,
}

impl Default for PipelineConfig {
    fn default() -> PipelineConfig {
        PipelineConfig {
            instruments: vec![1],
            multiplier: 50.0,
            order_qty: 1,
            signal: SignalConfig::default(),
            risk: RiskConfig::default(),
        }
    }
}

/// What happened on one processed packet (for tests / diagnostics).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TickOutcome {
    pub signal: bool,
    pub order: bool,
    pub reject: bool,
}

/// The connector pipeline for one consumer thread.
pub struct Pipeline {
    book: OrderBook,
    signal: SignalEngine,
    risk: RiskManager,
    gateway: MockGateway,
    timer: Timer,
    metrics: MetricsSink,
    order_qty: i64,
}

impl Pipeline {
    pub fn new(cfg: PipelineConfig, metrics: MetricsSink, timer: Timer) -> Pipeline {
        let multipliers: Vec<(InstrumentId, f64)> =
            cfg.instruments.iter().map(|&id| (id, cfg.multiplier)).collect();
        Pipeline {
            book: OrderBook::with_instruments(&cfg.instruments),
            signal: SignalEngine::new(cfg.signal, &cfg.instruments),
            risk: RiskManager::new(cfg.risk, &multipliers),
            gateway: MockGateway::new(),
            timer,
            metrics,
            order_qty: cfg.order_qty,
        }
    }

    /// Total realized + unrealized PnL.
    pub fn total_pnl(&self) -> f64 {
        self.risk.total_pnl()
    }

    /// Orders accepted by the gateway so far.
    pub fn orders_sent(&self) -> u64 {
        self.gateway.accepted()
    }

    /// Process one packet. `t0` is the raw timestamp taken at ring-pop.
    pub fn process(&mut self, packet: &[u8], t0: u64) -> TickOutcome {
        // --- DECODE: parse the packet and collect its entries. ---
        let mut entries: ArrayVec<MdEntry, MAX_ENTRIES_PER_PACKET> = ArrayVec::new();
        if let Some((_, body)) = PacketHeader::parse(packet) {
            for m in messages(body) {
                if let Some(refresh) = IncrementalRefresh::decode(m.header, m.body) {
                    for e in refresh.entries() {
                        let _ = entries.try_push(e);
                    }
                }
            }
        }
        let t1 = self.timer.raw();

        // --- BOOK: apply entries, remember the last updated BBO. ---
        let mut last: Option<(InstrumentId, Bbo)> = None;
        for e in &entries {
            if let Some(bbo) = self.book.apply(e) {
                last = Some((e.security_id, bbo));
            }
        }
        let t2 = self.timer.raw();

        let mut outcome = TickOutcome::default();
        let (inst, bbo) = match last {
            Some(x) => x,
            None => {
                // No tradeable update (unknown instrument / no BBO yet); still
                // record the decode+book cost as a per-tick sample.
                let t3 = self.timer.raw();
                self.metrics.record(&StageSample {
                    decode_ns: self.timer.delta_ns(t0, t1),
                    book_ns: self.timer.delta_ns(t1, t2),
                    tick_to_signal_ns: self.timer.delta_ns(t0, t3),
                    ..Default::default()
                });
                return outcome;
            }
        };

        // --- SIGNAL: mark PnL and evaluate the mean-reversion signal. ---
        let mut sig = None;
        if let Some(mid) = bbo.mid() {
            self.risk.mark(inst, mid);
            sig = self.signal.update(inst, mid);
        }
        let t3 = self.timer.raw();

        let mut risk_ns = None;
        let mut gateway_ns = None;
        let mut tick_to_order_ns = None;

        if let Some(signal) = sig {
            outcome.signal = true;
            // Post at the mid (midpoint peg). The mock gateway fills a mid order
            // at its limit (the mid) rather than at the touch, so a naive taker
            // does not pay the spread on every round trip and therefore does not
            // structurally bleed PnL into the daily-loss kill-switch. The
            // kill-switch then trips only on genuine drawdowns, keeping order flow
            // (and the tick-to-order latency stream) alive throughout a run.
            if let Some(mid) = bbo.mid() {
                let px_raw = crate::f64_to_price9(mid);
                let req = OrderRequest {
                    instrument: inst,
                    side: signal.side,
                    qty: self.order_qty,
                    px_raw,
                };

                // --- RISK ---
                let decision = self.risk.check(&req, mid);
                let t4 = self.timer.raw();
                risk_ns = Some(self.timer.delta_ns(t3, t4));

                match decision {
                    Ok(()) => {
                        // --- GATEWAY ---
                        let (_ack, fill) = self.gateway.submit(&req, &bbo);
                        self.risk
                            .on_fill(fill.instrument, fill.side, fill.qty, fill.fill_px_raw);
                        let t5 = self.timer.raw();
                        gateway_ns = Some(self.timer.delta_ns(t4, t5));
                        tick_to_order_ns = Some(self.timer.delta_ns(t0, t5));
                        outcome.order = true;
                        self.metrics.inc_order();
                        // Position only changes on a fill; publish the new gauge
                        // off the measured hot path (after t5 is stamped).
                        self.metrics.set_position(self.risk.net_position_total());
                    }
                    Err(_reason) => {
                        outcome.reject = true;
                        self.metrics.inc_reject();
                    }
                }
            }
        }

        self.metrics.record(&StageSample {
            decode_ns: self.timer.delta_ns(t0, t1),
            book_ns: self.timer.delta_ns(t1, t2),
            signal_ns: self.timer.delta_ns(t2, t3),
            tick_to_signal_ns: self.timer.delta_ns(t0, t3),
            risk_ns,
            gateway_ns,
            tick_to_order_ns,
        });

        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loadgen::{Generator, GeneratorConfig};
    use crate::metrics::new_metrics;

    fn pipeline(cfg: PipelineConfig) -> Pipeline {
        let (sink, _rep) = new_metrics(1000, 42);
        Pipeline::new(cfg, sink, Timer::new())
    }

    fn gen_for(instruments: Vec<InstrumentId>) -> Generator {
        Generator::new(GeneratorConfig {
            instruments,
            ..GeneratorConfig::default()
        })
    }

    #[test]
    fn processes_a_generated_stream_and_emits_orders() {
        let cfg = PipelineConfig {
            instruments: vec![1],
            ..Default::default()
        };
        let mut p = pipeline(cfg);
        let mut g = gen_for(vec![1]);
        let mut signals = 0;
        let mut orders = 0;
        for _ in 0..8000 {
            let packet = g.next_packet();
            let t0 = p.timer.raw();
            let out = p.process(&packet, t0);
            signals += out.signal as u32;
            orders += out.order as u32;
        }
        assert!(signals > 0, "the strategy should fire signals");
        assert!(orders > 0, "passing signals should reach the gateway");
        assert_eq!(p.orders_sent(), orders as u64);
    }

    #[test]
    fn risk_rejections_block_orders() {
        // A zero position limit rejects every order (projected |1| > 0).
        let cfg = PipelineConfig {
            instruments: vec![1],
            risk: RiskConfig {
                position_limit: 0,
                ..RiskConfig::default()
            },
            ..Default::default()
        };
        let mut p = pipeline(cfg);
        let mut g = gen_for(vec![1]);
        let mut rejects = 0;
        for _ in 0..8000 {
            let packet = g.next_packet();
            let t0 = p.timer.raw();
            let out = p.process(&packet, t0);
            rejects += out.reject as u32;
            assert!(!out.order, "no order should pass a zero-width band");
        }
        assert!(rejects > 0, "expected risk rejections");
        assert_eq!(p.orders_sent(), 0);
    }

    #[test]
    fn garbage_input_is_handled_without_panic() {
        let mut p = pipeline(PipelineConfig::default());
        let t0 = p.timer.raw();
        assert_eq!(p.process(&[0xFF, 0x00, 0x01], t0), TickOutcome::default());
        let t0 = p.timer.raw();
        assert_eq!(p.process(&[], t0), TickOutcome::default());
    }
}
