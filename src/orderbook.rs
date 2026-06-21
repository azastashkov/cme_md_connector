//! Per-instrument depth book built from `MdEntry` updates; derives the BBO.
//!
//! Each side is a fixed-depth ladder (no hot-path allocation) indexed by
//! `MDPriceLevel - 1` (0 = top of book). Only outright bid/offer entries update
//! the book; implied (`E`/`F`) entries are ignored, and a `BookReset` (`J`)
//! clears both sides.

use std::collections::HashMap;

use arrayvec::ArrayVec;

use crate::mdp3::book_refresh::MdEntry;
use crate::mdp3::enums::{MdEntryType, MdUpdateAction};
use crate::{price9_to_f64, InstrumentId, Side};

/// Maximum tracked depth per side.
pub const MAX_DEPTH: usize = 10;

/// A single price level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    pub px_raw: i64,
    pub size: i32,
    pub num_orders: i32,
}

/// Top-of-book snapshot for one instrument.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Bbo {
    pub bid_px_raw: Option<i64>,
    pub bid_size: i32,
    pub offer_px_raw: Option<i64>,
    pub offer_size: i32,
}

impl Bbo {
    /// Mid price as a float, when both sides are present.
    #[inline]
    pub fn mid(&self) -> Option<f64> {
        match (self.bid_px_raw, self.offer_px_raw) {
            (Some(b), Some(o)) => Some((price9_to_f64(b) + price9_to_f64(o)) / 2.0),
            _ => None,
        }
    }
}

/// One instrument's two-sided depth book.
#[derive(Debug, Default)]
pub struct InstrumentBook {
    bids: ArrayVec<Level, MAX_DEPTH>,
    offers: ArrayVec<Level, MAX_DEPTH>,
    last_rpt_seq: Option<u32>,
    gaps: u64,
}

impl InstrumentBook {
    pub fn new() -> InstrumentBook {
        InstrumentBook::default()
    }

    /// Apply one decoded entry to this book.
    pub fn apply(&mut self, e: &MdEntry) {
        // Per-instrument RptSeq gap detection (informational; does not block).
        if let Some(last) = self.last_rpt_seq {
            if e.rpt_seq > last + 1 {
                self.gaps += 1;
            }
        }
        self.last_rpt_seq = Some(e.rpt_seq);

        if e.entry_type == MdEntryType::BookReset {
            self.bids.clear();
            self.offers.clear();
            return;
        }

        // Only outright bid/offer entries update this book; implied are ignored.
        let side = match e.entry_type {
            MdEntryType::Bid => Side::Bid,
            MdEntryType::Offer => Side::Offer,
            _ => return,
        };

        let idx = (e.price_level as usize).saturating_sub(1);
        let level = Level {
            px_raw: e.px_raw,
            size: e.size,
            num_orders: e.num_orders,
        };
        let levels = match side {
            Side::Bid => &mut self.bids,
            Side::Offer => &mut self.offers,
        };

        match e.update_action {
            MdUpdateAction::New => {
                if idx > levels.len() {
                    return; // can't insert past the end+1
                }
                if levels.is_full() {
                    levels.pop(); // drop the deepest level to make room
                }
                if idx <= levels.len() {
                    levels.insert(idx, level);
                }
            }
            MdUpdateAction::Change | MdUpdateAction::Overlay => {
                if idx < levels.len() {
                    levels[idx] = level;
                } else if idx == levels.len() && !levels.is_full() {
                    levels.push(level);
                }
            }
            MdUpdateAction::Delete => {
                if idx < levels.len() {
                    levels.remove(idx);
                }
            }
            MdUpdateAction::DeleteThru => {
                levels.clear();
            }
            MdUpdateAction::DeleteFrom => {
                levels.truncate(idx);
            }
        }
    }

    /// Current best bid/offer.
    pub fn bbo(&self) -> Bbo {
        Bbo {
            bid_px_raw: self.bids.first().map(|l| l.px_raw),
            bid_size: self.bids.first().map_or(0, |l| l.size),
            offer_px_raw: self.offers.first().map(|l| l.px_raw),
            offer_size: self.offers.first().map_or(0, |l| l.size),
        }
    }

    /// Count of detected `RptSeq` discontinuities for this instrument.
    pub fn gaps(&self) -> u64 {
        self.gaps
    }

    /// Depth of one side (for tests / diagnostics).
    pub fn depth(&self, side: Side) -> usize {
        match side {
            Side::Bid => self.bids.len(),
            Side::Offer => self.offers.len(),
        }
    }
}

/// A collection of instrument books keyed by `SecurityID`, preallocated at
/// startup so the hot path performs no map growth.
#[derive(Debug, Default)]
pub struct OrderBook {
    books: HashMap<InstrumentId, InstrumentBook>,
}

impl OrderBook {
    /// Preallocate books for a known set of instruments.
    pub fn with_instruments(ids: &[InstrumentId]) -> OrderBook {
        let mut books = HashMap::with_capacity(ids.len());
        for &id in ids {
            books.insert(id, InstrumentBook::new());
        }
        OrderBook { books }
    }

    /// Apply an entry to the book for its `security_id`, returning the updated
    /// BBO. `None` if the instrument was not registered.
    pub fn apply(&mut self, e: &MdEntry) -> Option<Bbo> {
        let b = self.books.get_mut(&e.security_id)?;
        b.apply(e);
        Some(b.bbo())
    }

    /// Borrow an instrument's book.
    pub fn book(&self, id: InstrumentId) -> Option<&InstrumentBook> {
        self.books.get(&id)
    }

    /// Total `RptSeq` gaps across all instruments.
    pub fn total_gaps(&self) -> u64 {
        self.books.values().map(|b| b.gaps()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        action: MdUpdateAction,
        ty: MdEntryType,
        level: u8,
        px_raw: i64,
        size: i32,
        rpt_seq: u32,
    ) -> MdEntry {
        MdEntry {
            security_id: 1,
            rpt_seq,
            px_raw,
            size,
            num_orders: 1,
            price_level: level,
            update_action: action,
            entry_type: ty,
        }
    }

    #[test]
    fn new_entries_set_the_bbo() {
        let mut b = InstrumentBook::new();
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 1, 100, 5, 1));
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Offer, 1, 101, 7, 2));
        let bbo = b.bbo();
        assert_eq!(bbo.bid_px_raw, Some(100));
        assert_eq!(bbo.bid_size, 5);
        assert_eq!(bbo.offer_px_raw, Some(101));
        assert_eq!(bbo.offer_size, 7);
    }

    #[test]
    fn new_inserts_shift_deeper_levels_down() {
        let mut b = InstrumentBook::new();
        // Insert top bid 100, then a new better bid 101 at level 1 pushes 100 to level 2.
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 1, 100, 5, 1));
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 1, 101, 6, 2));
        assert_eq!(b.bbo().bid_px_raw, Some(101));
        assert_eq!(b.depth(Side::Bid), 2);
    }

    #[test]
    fn change_updates_size_at_a_level() {
        let mut b = InstrumentBook::new();
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 1, 100, 5, 1));
        b.apply(&entry(MdUpdateAction::Change, MdEntryType::Bid, 1, 100, 9, 2));
        assert_eq!(b.bbo().bid_size, 9);
        assert_eq!(b.depth(Side::Bid), 1);
    }

    #[test]
    fn delete_removes_a_level_and_shifts_up() {
        let mut b = InstrumentBook::new();
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 1, 101, 6, 1));
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 2, 100, 5, 2));
        // Delete the top level -> the deeper level becomes the top.
        b.apply(&entry(MdUpdateAction::Delete, MdEntryType::Bid, 1, 101, 6, 3));
        assert_eq!(b.bbo().bid_px_raw, Some(100));
        assert_eq!(b.depth(Side::Bid), 1);
    }

    #[test]
    fn delete_thru_clears_the_side() {
        let mut b = InstrumentBook::new();
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 1, 101, 6, 1));
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 2, 100, 5, 2));
        b.apply(&entry(MdUpdateAction::DeleteThru, MdEntryType::Bid, 1, 0, 0, 3));
        assert_eq!(b.depth(Side::Bid), 0);
        assert_eq!(b.bbo().bid_px_raw, None);
    }

    #[test]
    fn delete_from_truncates_from_a_level_down() {
        let mut b = InstrumentBook::new();
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 1, 102, 6, 1));
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 2, 101, 5, 2));
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 3, 100, 4, 3));
        // Delete from level 2 down -> only the top level remains.
        b.apply(&entry(MdUpdateAction::DeleteFrom, MdEntryType::Bid, 2, 0, 0, 4));
        assert_eq!(b.depth(Side::Bid), 1);
        assert_eq!(b.bbo().bid_px_raw, Some(102));
    }

    #[test]
    fn overlay_replaces_a_level() {
        let mut b = InstrumentBook::new();
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Offer, 1, 101, 7, 1));
        b.apply(&entry(MdUpdateAction::Overlay, MdEntryType::Offer, 1, 102, 3, 2));
        assert_eq!(b.bbo().offer_px_raw, Some(102));
        assert_eq!(b.bbo().offer_size, 3);
    }

    #[test]
    fn book_reset_clears_both_sides() {
        let mut b = InstrumentBook::new();
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 1, 100, 5, 1));
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Offer, 1, 101, 7, 2));
        b.apply(&entry(MdUpdateAction::New, MdEntryType::BookReset, 1, 0, 0, 3));
        assert_eq!(b.bbo(), Bbo::default());
    }

    #[test]
    fn implied_entries_are_ignored() {
        let mut b = InstrumentBook::new();
        b.apply(&entry(MdUpdateAction::New, MdEntryType::ImpliedBid, 1, 100, 5, 1));
        assert_eq!(b.depth(Side::Bid), 0);
    }

    #[test]
    fn detects_rpt_seq_gaps() {
        let mut b = InstrumentBook::new();
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 1, 100, 5, 1));
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 2, 99, 5, 2));
        // Jump from 2 to 4: one gap.
        b.apply(&entry(MdUpdateAction::New, MdEntryType::Bid, 3, 98, 5, 4));
        assert_eq!(b.gaps(), 1);
    }

    #[test]
    fn mid_price_is_average_of_touch() {
        let bbo = Bbo {
            bid_px_raw: Some(crate::f64_to_price9(100.0)),
            bid_size: 1,
            offer_px_raw: Some(crate::f64_to_price9(102.0)),
            offer_size: 1,
        };
        assert_eq!(bbo.mid(), Some(101.0));
    }

    #[test]
    fn order_book_routes_by_security_id() {
        let mut ob = OrderBook::with_instruments(&[1, 2]);
        let mut e = entry(MdUpdateAction::New, MdEntryType::Bid, 1, 100, 5, 1);
        e.security_id = 2;
        let bbo = ob.apply(&e).expect("instrument 2 is registered");
        assert_eq!(bbo.bid_px_raw, Some(100));
        // Instrument 1 is untouched.
        assert_eq!(ob.book(1).unwrap().bbo(), Bbo::default());
    }

    #[test]
    fn order_book_ignores_unregistered_instrument() {
        let mut ob = OrderBook::with_instruments(&[1]);
        let mut e = entry(MdUpdateAction::New, MdEntryType::Bid, 1, 100, 5, 1);
        e.security_id = 99;
        assert!(ob.apply(&e).is_none());
    }
}
