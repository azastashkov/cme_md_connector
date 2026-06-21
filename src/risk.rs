//! Pre-trade risk management: three sequential checks plus position/PnL state.
//!
//! Checks run cheapest-first; the first failure rejects the order before it can
//! reach the gateway:
//! 1. **Price Reasonableness** — order price within a tick band around the mid.
//! 2. **Position Limit** — projected |net position| within a per-book cap.
//! 3. **Daily Loss Limit** — a latching kill-switch on realized+unrealized PnL.

use std::collections::HashMap;

use thiserror::Error;

use crate::{price9_to_f64, InstrumentId, OrderSide};

/// Risk limits and price-band configuration.
#[derive(Debug, Clone, Copy)]
pub struct RiskConfig {
    /// Max absolute net position per instrument (contracts).
    pub position_limit: i64,
    /// Loss cap; the kill-switch latches when total PnL <= -this.
    pub daily_loss_limit: f64,
    /// Max allowed deviation of an order price from the mid, in ticks.
    pub price_band_ticks: i64,
    /// Tick size (price units).
    pub tick: f64,
}

impl Default for RiskConfig {
    fn default() -> RiskConfig {
        RiskConfig {
            position_limit: 100,
            daily_loss_limit: 10_000.0,
            price_band_ticks: 20,
            tick: 0.25,
        }
    }
}

/// A candidate order to be risk-checked.
#[derive(Debug, Clone, Copy)]
pub struct OrderRequest {
    pub instrument: InstrumentId,
    pub side: OrderSide,
    pub qty: i64,
    /// Limit price as a raw `PRICE9` integer.
    pub px_raw: i64,
}

/// Why an order was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Error)]
pub enum RejectReason {
    #[error("price {px} outside reasonable band around mid")]
    PriceUnreasonable { px: i64 },
    #[error("position limit exceeded: projected {projected}, limit {limit}")]
    PositionLimit { projected: i64, limit: i64 },
    #[error("daily loss limit breached: pnl {pnl}")]
    DailyLossLimit { pnl: f64 },
}

#[derive(Debug, Clone, Copy)]
struct Position {
    net: i64,
    avg_px: f64,
    realized: f64,
    mark: f64,
    multiplier: f64,
}

impl Position {
    fn new(multiplier: f64) -> Position {
        Position {
            net: 0,
            avg_px: 0.0,
            realized: 0.0,
            mark: 0.0,
            multiplier,
        }
    }

    fn unrealized(&self) -> f64 {
        (self.mark - self.avg_px) * self.net as f64 * self.multiplier
    }

    fn pnl(&self) -> f64 {
        self.realized + self.unrealized()
    }
}

/// Pre-trade risk manager.
pub struct RiskManager {
    cfg: RiskConfig,
    positions: HashMap<InstrumentId, Position>,
    killed: bool,
}

impl RiskManager {
    /// Build a risk manager for a set of `(instrument, contract_multiplier)`.
    pub fn new(cfg: RiskConfig, instruments: &[(InstrumentId, f64)]) -> RiskManager {
        let mut positions = HashMap::with_capacity(instruments.len());
        for &(id, mult) in instruments {
            positions.insert(id, Position::new(mult));
        }
        RiskManager {
            cfg,
            positions,
            killed: false,
        }
    }

    /// Run the three pre-trade checks against the current `mid`.
    pub fn check(&self, req: &OrderRequest, mid: f64) -> Result<(), RejectReason> {
        // 1. Price reasonableness — order price within a tick band around mid.
        let deviation = (price9_to_f64(req.px_raw) - mid).abs();
        let max_deviation = self.cfg.price_band_ticks as f64 * self.cfg.tick;
        if deviation > max_deviation {
            return Err(RejectReason::PriceUnreasonable { px: req.px_raw });
        }

        // 2. Position limit on the projected net position.
        let net = self.net_position(req.instrument);
        let projected = net + req.side.sign() * req.qty;
        if projected.abs() > self.cfg.position_limit {
            return Err(RejectReason::PositionLimit {
                projected,
                limit: self.cfg.position_limit,
            });
        }

        // 3. Daily loss limit — once latched, block risk-increasing orders but
        //    still allow flattening/reducing existing exposure.
        if self.killed && projected.abs() > net.abs() {
            return Err(RejectReason::DailyLossLimit {
                pnl: self.total_pnl(),
            });
        }

        Ok(())
    }

    /// Update an instrument's mark (mid) — feeds unrealized PnL and the band.
    pub fn mark(&mut self, instrument: InstrumentId, mid: f64) {
        if let Some(p) = self.positions.get_mut(&instrument) {
            p.mark = mid;
        }
        self.refresh_kill();
    }

    /// Apply a fill: update net position, average price, and realized PnL
    /// (average-cost accounting).
    pub fn on_fill(&mut self, instrument: InstrumentId, side: OrderSide, qty: i64, fill_px_raw: i64) {
        if let Some(p) = self.positions.get_mut(&instrument) {
            let fill_px = price9_to_f64(fill_px_raw);
            let signed = side.sign() * qty;
            let new_net = p.net + signed;

            let same_direction = p.net == 0 || (p.net > 0) == (signed > 0);
            if same_direction {
                // Opening or adding: quantity-weight the average price.
                let cost = p.avg_px * p.net.abs() as f64 + fill_px * qty as f64;
                let abs = new_net.abs() as f64;
                p.avg_px = if abs > 0.0 { cost / abs } else { 0.0 };
            } else {
                // Reducing, closing, or flipping: realize on the closed amount.
                let closed = qty.min(p.net.abs());
                let dir = if p.net > 0 { 1.0 } else { -1.0 };
                p.realized += (fill_px - p.avg_px) * closed as f64 * dir * p.multiplier;
                if signed.abs() > p.net.abs() {
                    // Flipped past flat: the remainder opens at the fill price.
                    p.avg_px = fill_px;
                } else if new_net == 0 {
                    p.avg_px = 0.0;
                }
            }
            p.net = new_net;
        }
        self.refresh_kill();
    }

    /// Latch the kill-switch once total PnL breaches the daily loss limit.
    fn refresh_kill(&mut self) {
        if self.total_pnl() <= -self.cfg.daily_loss_limit {
            self.killed = true;
        }
    }

    /// Net position for an instrument (0 if unknown).
    pub fn net_position(&self, instrument: InstrumentId) -> i64 {
        self.positions.get(&instrument).map_or(0, |p| p.net)
    }

    /// Total realized + unrealized PnL across all instruments.
    pub fn total_pnl(&self) -> f64 {
        self.positions.values().map(|p| p.pnl()).sum()
    }

    /// Whether the daily-loss kill-switch has latched.
    pub fn is_killed(&self) -> bool {
        self.killed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::f64_to_price9;

    fn mgr() -> RiskManager {
        RiskManager::new(RiskConfig::default(), &[(1, 1.0)])
    }

    fn order(side: OrderSide, qty: i64, px: f64) -> OrderRequest {
        OrderRequest {
            instrument: 1,
            side,
            qty,
            px_raw: f64_to_price9(px),
        }
    }

    #[test]
    fn passes_a_reasonable_in_limit_order() {
        let m = mgr();
        assert_eq!(m.check(&order(OrderSide::Buy, 1, 100.0), 100.0), Ok(()));
    }

    #[test]
    fn rejects_price_far_from_mid() {
        let m = mgr();
        // 20 ticks * 0.25 = 5.0 band; 100 vs mid 80 is 20 away.
        let req = order(OrderSide::Buy, 1, 100.0);
        assert_eq!(
            m.check(&req, 80.0),
            Err(RejectReason::PriceUnreasonable { px: req.px_raw })
        );
    }

    #[test]
    fn rejects_order_breaching_position_limit() {
        let m = RiskManager::new(
            RiskConfig {
                position_limit: 5,
                ..RiskConfig::default()
            },
            &[(1, 1.0)],
        );
        let req = order(OrderSide::Buy, 6, 100.0);
        assert_eq!(
            m.check(&req, 100.0),
            Err(RejectReason::PositionLimit {
                projected: 6,
                limit: 5
            })
        );
    }

    #[test]
    fn position_limit_allows_risk_reducing_order() {
        let mut m = RiskManager::new(
            RiskConfig {
                position_limit: 5,
                ..RiskConfig::default()
            },
            &[(1, 1.0)],
        );
        // Go long 5 (at the limit), then a sell that reduces is allowed.
        m.on_fill(1, OrderSide::Buy, 5, f64_to_price9(100.0));
        assert_eq!(m.net_position(1), 5);
        assert_eq!(m.check(&order(OrderSide::Sell, 2, 100.0), 100.0), Ok(()));
    }

    #[test]
    fn realizes_pnl_on_a_round_trip() {
        let mut m = mgr();
        m.on_fill(1, OrderSide::Buy, 10, f64_to_price9(100.0));
        m.on_fill(1, OrderSide::Sell, 10, f64_to_price9(102.0));
        assert_eq!(m.net_position(1), 0);
        // (102 - 100) * 10 * 1.0 = 20
        assert!((m.total_pnl() - 20.0).abs() < 1e-6);
    }

    #[test]
    fn marks_unrealized_pnl_to_current_mid() {
        let mut m = mgr();
        m.on_fill(1, OrderSide::Buy, 10, f64_to_price9(100.0));
        m.mark(1, 101.0);
        // (101 - 100) * 10 = 10 unrealized
        assert!((m.total_pnl() - 10.0).abs() < 1e-6);
    }

    #[test]
    fn applies_contract_multiplier_to_pnl() {
        let mut m = RiskManager::new(RiskConfig::default(), &[(1, 50.0)]);
        m.on_fill(1, OrderSide::Buy, 2, f64_to_price9(5000.0));
        m.on_fill(1, OrderSide::Sell, 2, f64_to_price9(5001.0));
        // (5001 - 5000) * 2 * 50 = 100
        assert!((m.total_pnl() - 100.0).abs() < 1e-6);
    }

    #[test]
    fn daily_loss_kill_switch_latches_blocks_increasing_allows_flatten() {
        let mut m = RiskManager::new(
            RiskConfig {
                position_limit: 1000, // keep the position check out of the way
                daily_loss_limit: 50.0,
                ..RiskConfig::default()
            },
            &[(1, 1.0)],
        );
        // Long 100 @ 100, then mark down to 99 -> -100 unrealized < -50.
        m.on_fill(1, OrderSide::Buy, 100, f64_to_price9(100.0));
        m.mark(1, 99.0);
        assert!(m.is_killed());

        // A risk-increasing order is blocked by the loss limit...
        match m.check(&order(OrderSide::Buy, 1, 99.0), 99.0) {
            Err(RejectReason::DailyLossLimit { .. }) => {}
            other => panic!("expected loss-limit reject, got {other:?}"),
        }
        // ...but flattening is still permitted.
        assert_eq!(m.check(&order(OrderSide::Sell, 10, 99.0), 99.0), Ok(()));

        // Latched: recovering the mark does not un-kill.
        m.mark(1, 100.0);
        assert!(m.is_killed());
    }
}
