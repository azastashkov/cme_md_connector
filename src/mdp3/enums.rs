//! MDP 3.0 enumerations used to build the book.

use crate::Side;

/// `MDUpdateAction` (tag 279) — how a price-level entry mutates the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MdUpdateAction {
    New = 0,
    Change = 1,
    Delete = 2,
    DeleteThru = 3,
    DeleteFrom = 4,
    Overlay = 5,
}

impl MdUpdateAction {
    /// Decode from the on-wire `u8`; `None` for unknown values.
    #[inline]
    pub fn from_u8(v: u8) -> Option<MdUpdateAction> {
        match v {
            0 => Some(MdUpdateAction::New),
            1 => Some(MdUpdateAction::Change),
            2 => Some(MdUpdateAction::Delete),
            3 => Some(MdUpdateAction::DeleteThru),
            4 => Some(MdUpdateAction::DeleteFrom),
            5 => Some(MdUpdateAction::Overlay),
            _ => None,
        }
    }
}

/// `MDEntryType` (tag 269) — which book (side / implied / reset) an entry targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdEntryType {
    Bid,
    Offer,
    ImpliedBid,
    ImpliedOffer,
    BookReset,
}

impl MdEntryType {
    /// Decode from the on-wire ASCII byte; `None` for unrecognised types.
    #[inline]
    pub fn from_byte(b: u8) -> Option<MdEntryType> {
        match b {
            b'0' => Some(MdEntryType::Bid),
            b'1' => Some(MdEntryType::Offer),
            b'E' => Some(MdEntryType::ImpliedBid),
            b'F' => Some(MdEntryType::ImpliedOffer),
            b'J' => Some(MdEntryType::BookReset),
            _ => None,
        }
    }

    /// On-wire ASCII byte for this entry type.
    #[inline]
    pub fn as_byte(self) -> u8 {
        match self {
            MdEntryType::Bid => b'0',
            MdEntryType::Offer => b'1',
            MdEntryType::ImpliedBid => b'E',
            MdEntryType::ImpliedOffer => b'F',
            MdEntryType::BookReset => b'J',
        }
    }

    /// The book side this entry belongs to, if any (`BookReset` has none).
    #[inline]
    pub fn side(self) -> Option<Side> {
        match self {
            MdEntryType::Bid | MdEntryType::ImpliedBid => Some(Side::Bid),
            MdEntryType::Offer | MdEntryType::ImpliedOffer => Some(Side::Offer),
            MdEntryType::BookReset => None,
        }
    }
}

/// `MatchEventIndicator` (tag 5799) bitset carried in the message root block.
#[derive(Debug, Clone, Copy)]
pub struct MatchEventIndicator(pub u8);

impl MatchEventIndicator {
    /// End-of-event bit (0x80): the book is consistent and the BBO may be
    /// published once this is set on the last message of a matching event.
    pub const END_OF_EVENT: u8 = 0x80;

    /// Whether this message ends the matching event.
    #[inline]
    pub fn is_end_of_event(self) -> bool {
        self.0 & Self::END_OF_EVENT != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_every_update_action() {
        assert_eq!(MdUpdateAction::from_u8(0), Some(MdUpdateAction::New));
        assert_eq!(MdUpdateAction::from_u8(1), Some(MdUpdateAction::Change));
        assert_eq!(MdUpdateAction::from_u8(2), Some(MdUpdateAction::Delete));
        assert_eq!(MdUpdateAction::from_u8(3), Some(MdUpdateAction::DeleteThru));
        assert_eq!(MdUpdateAction::from_u8(4), Some(MdUpdateAction::DeleteFrom));
        assert_eq!(MdUpdateAction::from_u8(5), Some(MdUpdateAction::Overlay));
    }

    #[test]
    fn rejects_unknown_update_action() {
        assert_eq!(MdUpdateAction::from_u8(6), None);
        assert_eq!(MdUpdateAction::from_u8(255), None);
    }

    #[test]
    fn decodes_entry_types_and_round_trips_byte() {
        for ty in [
            MdEntryType::Bid,
            MdEntryType::Offer,
            MdEntryType::ImpliedBid,
            MdEntryType::ImpliedOffer,
            MdEntryType::BookReset,
        ] {
            assert_eq!(MdEntryType::from_byte(ty.as_byte()), Some(ty));
        }
    }

    #[test]
    fn rejects_unknown_entry_type() {
        assert_eq!(MdEntryType::from_byte(b'Z'), None);
    }

    #[test]
    fn maps_entry_type_to_book_side() {
        assert_eq!(MdEntryType::Bid.side(), Some(Side::Bid));
        assert_eq!(MdEntryType::ImpliedBid.side(), Some(Side::Bid));
        assert_eq!(MdEntryType::Offer.side(), Some(Side::Offer));
        assert_eq!(MdEntryType::ImpliedOffer.side(), Some(Side::Offer));
        assert_eq!(MdEntryType::BookReset.side(), None);
    }

    #[test]
    fn detects_end_of_event_bit() {
        assert!(MatchEventIndicator(0x80).is_end_of_event());
        assert!(MatchEventIndicator(0x81).is_end_of_event());
        assert!(!MatchEventIndicator(0x01).is_end_of_event());
        assert!(!MatchEventIndicator(0x00).is_end_of_event());
    }
}
