//! Statistical-arbitrage signal: single-instrument EWMA mean-reversion.
//!
//! Each instrument's mid feeds an exponentially-weighted mean and variance
//! (O(1) time and space). The z-score `z = (mid - mean) / std` drives a small
//! state machine: enter against the deviation at `|z| >= z_entry`, flatten near
//! the mean at `|z| <= z_exit`, and stop out at `|z| >= z_stop`.

use std::collections::HashMap;

use crate::{InstrumentId, OrderSide};

/// Signal-engine parameters.
#[derive(Debug, Clone, Copy)]
pub struct SignalConfig {
    /// EWMA smoothing factor in (0, 1]; ~2/alpha-1 effective samples.
    pub alpha: f64,
    /// Entry threshold (enter when |z| >= this).
    pub z_entry: f64,
    /// Exit threshold (flatten when |z| <= this).
    pub z_exit: f64,
    /// Stop threshold (force flat when |z| >= this).
    pub z_stop: f64,
    /// Samples to observe before emitting any signal.
    pub warmup: u32,
}

impl Default for SignalConfig {
    fn default() -> SignalConfig {
        SignalConfig {
            alpha: 0.1,
            z_entry: 2.0,
            z_exit: 0.5,
            z_stop: 3.0,
            warmup: 20,
        }
    }
}

/// Whether a signal opens or closes a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalKind {
    Enter,
    Exit,
}

/// A trade signal for one instrument.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Signal {
    pub instrument: InstrumentId,
    pub kind: SignalKind,
    pub side: OrderSide,
    pub z: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pos {
    Flat,
    Long,
    Short,
}

/// Per-instrument EWMA + position state.
#[derive(Debug, Clone)]
struct State {
    mean: f64,
    var: f64,
    count: u32,
    pos: Pos,
}

impl State {
    fn new() -> State {
        State {
            mean: 0.0,
            var: 0.0,
            count: 0,
            pos: Pos::Flat,
        }
    }
}

/// Mean-reversion signal engine across a set of instruments.
pub struct SignalEngine {
    cfg: SignalConfig,
    states: HashMap<InstrumentId, State>,
}

impl SignalEngine {
    /// Build an engine with preallocated per-instrument state.
    pub fn new(cfg: SignalConfig, instruments: &[InstrumentId]) -> SignalEngine {
        let mut states = HashMap::with_capacity(instruments.len());
        for &id in instruments {
            states.insert(id, State::new());
        }
        SignalEngine { cfg, states }
    }

    /// Feed a new mid for `instrument`; returns a signal if one fires.
    pub fn update(&mut self, instrument: InstrumentId, mid: f64) -> Option<Signal> {
        let cfg = self.cfg;
        let s = self.states.get_mut(&instrument)?;

        // Standardize against the model's prediction *before* absorbing this
        // sample, so a fresh spike reads as a large z rather than being half
        // cancelled by its own update.
        let prev_mean = s.mean;
        let prev_std = s.var.sqrt();
        let delta = mid - s.mean;

        if s.count == 0 {
            s.mean = mid;
            s.var = 0.0;
        } else {
            s.mean += cfg.alpha * delta;
            s.var = (1.0 - cfg.alpha) * (s.var + cfg.alpha * delta * delta);
        }
        s.count += 1;

        if s.count <= cfg.warmup || prev_std < 1e-9 {
            return None;
        }

        let z = (mid - prev_mean) / prev_std;
        let abs_z = z.abs();
        let signal = |kind, side| {
            Some(Signal {
                instrument,
                kind,
                side,
                z,
            })
        };

        match s.pos {
            Pos::Flat => {
                if z >= cfg.z_entry {
                    s.pos = Pos::Short;
                    return signal(SignalKind::Enter, OrderSide::Sell);
                }
                if z <= -cfg.z_entry {
                    s.pos = Pos::Long;
                    return signal(SignalKind::Enter, OrderSide::Buy);
                }
            }
            Pos::Long => {
                if abs_z <= cfg.z_exit || abs_z >= cfg.z_stop {
                    s.pos = Pos::Flat;
                    return signal(SignalKind::Exit, OrderSide::Sell);
                }
            }
            Pos::Short => {
                if abs_z <= cfg.z_exit || abs_z >= cfg.z_stop {
                    s.pos = Pos::Flat;
                    return signal(SignalKind::Exit, OrderSide::Buy);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> SignalEngine {
        SignalEngine::new(SignalConfig::default(), &[1])
    }

    /// Feed `n` samples alternating around 100 to establish a non-zero variance.
    fn warmup(e: &mut SignalEngine, n: u32) {
        for i in 0..n {
            let mid = if i % 2 == 0 { 99.5 } else { 100.5 };
            assert!(e.update(1, mid).is_none(), "no signal during warmup");
        }
    }

    #[test]
    fn no_signal_before_warmup_completes() {
        let mut e = engine();
        for i in 0..SignalConfig::default().warmup {
            let mid = if i % 2 == 0 { 99.5 } else { 100.5 };
            assert_eq!(e.update(1, mid), None);
        }
    }

    #[test]
    fn enters_short_when_mid_spikes_high() {
        let mut e = engine();
        warmup(&mut e, 40);
        let sig = e.update(1, 110.0).expect("a large positive deviation fires");
        assert_eq!(sig.kind, SignalKind::Enter);
        assert_eq!(sig.side, OrderSide::Sell);
        assert!(sig.z >= SignalConfig::default().z_entry);
    }

    #[test]
    fn enters_long_when_mid_drops_low() {
        let mut e = engine();
        warmup(&mut e, 40);
        let sig = e.update(1, 90.0).expect("a large negative deviation fires");
        assert_eq!(sig.kind, SignalKind::Enter);
        assert_eq!(sig.side, OrderSide::Buy);
        assert!(sig.z <= -SignalConfig::default().z_entry);
    }

    #[test]
    fn does_not_reenter_while_in_position() {
        let mut e = engine();
        warmup(&mut e, 40);
        e.update(1, 110.0).expect("enter short");
        // Another high sample must not enter again.
        let again = e.update(1, 111.0);
        assert!(again.is_none() || again.unwrap().kind == SignalKind::Exit);
    }

    #[test]
    fn exits_when_z_reverts_toward_mean() {
        let mut e = engine();
        warmup(&mut e, 40);
        e.update(1, 110.0).expect("enter short");
        // Pull the mid back near the mean -> exit.
        let exit = e.update(1, 100.0).expect("reversion flattens");
        assert_eq!(exit.kind, SignalKind::Exit);
        assert_eq!(exit.side, OrderSide::Buy); // flatten a short by buying
    }

    #[test]
    fn unregistered_instrument_yields_no_signal() {
        let mut e = engine();
        assert_eq!(e.update(999, 100.0), None);
    }
}
