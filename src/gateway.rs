//! Mock order gateway.
//!
//! Accepts a risk-approved order, assigns a monotonic `OrderID`, acknowledges it
//! (`ExecType::New`), and simulates an immediate full fill: marketable orders
//! fill at the touch, otherwise at the limit price. The emitted [`Fill`] is fed
//! back into the risk manager's position/PnL state by the connector.

use crate::orderbook::Bbo;
use crate::risk::OrderRequest;
use crate::{InstrumentId, OrderSide};

/// FIX-style execution type (subset we model).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecType {
    New,
    Filled,
}

/// Acknowledgement that the gateway accepted an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    pub order_id: u64,
    pub exec_type: ExecType,
}

/// A simulated fill report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    pub order_id: u64,
    pub instrument: InstrumentId,
    pub side: OrderSide,
    pub qty: i64,
    /// Fill price as a raw `PRICE9` integer.
    pub fill_px_raw: i64,
}

/// A minimal in-process order gateway with deterministic immediate fills.
#[derive(Debug, Default)]
pub struct MockGateway {
    next_id: u64,
    accepted: u64,
}

impl MockGateway {
    pub fn new() -> MockGateway {
        MockGateway {
            next_id: 1,
            accepted: 0,
        }
    }

    /// Number of orders accepted so far.
    pub fn accepted(&self) -> u64 {
        self.accepted
    }

    /// Submit an order; returns the ack and the simulated fill.
    pub fn submit(&mut self, req: &OrderRequest, bbo: &Bbo) -> (Ack, Fill) {
        let order_id = self.next_id;
        self.next_id += 1;
        self.accepted += 1;

        // Marketable orders fill at the touch; otherwise at the limit price.
        let fill_px_raw = match req.side {
            OrderSide::Buy => match bbo.offer_px_raw {
                Some(offer) if req.px_raw >= offer => offer,
                _ => req.px_raw,
            },
            OrderSide::Sell => match bbo.bid_px_raw {
                Some(bid) if req.px_raw <= bid => bid,
                _ => req.px_raw,
            },
        };

        (
            Ack {
                order_id,
                exec_type: ExecType::New,
            },
            Fill {
                order_id,
                instrument: req.instrument,
                side: req.side,
                qty: req.qty,
                fill_px_raw,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::f64_to_price9;

    fn req(side: OrderSide, qty: i64, px: f64) -> OrderRequest {
        OrderRequest {
            instrument: 7,
            side,
            qty,
            px_raw: f64_to_price9(px),
        }
    }

    fn bbo(bid: f64, offer: f64) -> Bbo {
        Bbo {
            bid_px_raw: Some(f64_to_price9(bid)),
            bid_size: 10,
            offer_px_raw: Some(f64_to_price9(offer)),
            offer_size: 10,
        }
    }

    #[test]
    fn assigns_monotonic_order_ids_starting_at_one() {
        let mut g = MockGateway::new();
        let (a1, f1) = g.submit(&req(OrderSide::Buy, 1, 100.0), &bbo(99.0, 100.0));
        let (a2, _) = g.submit(&req(OrderSide::Buy, 1, 100.0), &bbo(99.0, 100.0));
        assert_eq!(a1.order_id, 1);
        assert_eq!(f1.order_id, 1);
        assert_eq!(a2.order_id, 2);
        assert_eq!(g.accepted(), 2);
    }

    #[test]
    fn acknowledges_with_new() {
        let mut g = MockGateway::new();
        let (ack, _) = g.submit(&req(OrderSide::Buy, 1, 100.0), &bbo(99.0, 100.0));
        assert_eq!(ack.exec_type, ExecType::New);
    }

    #[test]
    fn marketable_buy_fills_at_the_offer() {
        let mut g = MockGateway::new();
        // Buy priced through the offer fills at the offer (touch).
        let (_, fill) = g.submit(&req(OrderSide::Buy, 3, 101.0), &bbo(99.75, 100.0));
        assert_eq!(fill.fill_px_raw, f64_to_price9(100.0));
        assert_eq!(fill.qty, 3);
        assert_eq!(fill.side, OrderSide::Buy);
        assert_eq!(fill.instrument, 7);
    }

    #[test]
    fn non_marketable_buy_fills_at_limit() {
        let mut g = MockGateway::new();
        // Buy below the offer fills at its own limit.
        let (_, fill) = g.submit(&req(OrderSide::Buy, 1, 99.5), &bbo(99.25, 100.0));
        assert_eq!(fill.fill_px_raw, f64_to_price9(99.5));
    }

    #[test]
    fn marketable_sell_fills_at_the_bid() {
        let mut g = MockGateway::new();
        let (_, fill) = g.submit(&req(OrderSide::Sell, 2, 99.0), &bbo(99.5, 100.0));
        assert_eq!(fill.fill_px_raw, f64_to_price9(99.5));
        assert_eq!(fill.side, OrderSide::Sell);
    }
}
