//! HFT CME market-data connector + load test.
//!
//! Pipeline: raw MDP 3.0 / SBE packets -> decode -> BBO order book ->
//! mean-reversion signal -> pre-trade risk -> mock order gateway, with every
//! stage timed (p50/p95/p99 + total) and surfaced on a built-in dashboard.
//!
//! See `docs`/README for the architecture and the macOS measurement caveats.

pub mod gateway;
pub mod loadgen;
pub mod metrics;
pub mod mdp3;
pub mod orderbook;
pub mod risk;
pub mod signal;
pub mod timing;

/// CME `SecurityID` — identifies one instrument's book.
pub type InstrumentId = i32;

/// Side of the book / of an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Bid,
    Offer,
}

impl Side {
    /// The opposite side (bid <-> offer).
    #[inline]
    pub fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Offer,
            Side::Offer => Side::Bid,
        }
    }
}

/// Buy/sell direction of an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    /// The opposite direction (used to flatten a position).
    #[inline]
    pub fn opposite(self) -> OrderSide {
        match self {
            OrderSide::Buy => OrderSide::Sell,
            OrderSide::Sell => OrderSide::Buy,
        }
    }

    /// Signed position delta per filled contract (+1 buy, -1 sell).
    #[inline]
    pub fn sign(self) -> i64 {
        match self {
            OrderSide::Buy => 1,
            OrderSide::Sell => -1,
        }
    }
}

/// CME `PRICE9` fixed-point scale: on-the-wire integer = price * 1e9.
pub const PRICE9_SCALE: i64 = 1_000_000_000;

/// Convert a raw `PRICE9` integer (as carried in `MDEntryPx`) to a float price.
#[inline]
pub fn price9_to_f64(raw: i64) -> f64 {
    raw as f64 / PRICE9_SCALE as f64
}

/// Convert a float price to a raw `PRICE9` integer.
#[inline]
pub fn f64_to_price9(px: f64) -> i64 {
    (px * PRICE9_SCALE as f64).round() as i64
}
