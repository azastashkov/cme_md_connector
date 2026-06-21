//! `MDIncrementalRefreshBook` decode (templates 46 and legacy 32).
//!
//! Root block: `TransactTime u64 @0`, `MatchEventIndicator u8 @8`, padding to
//! `blockLength` (11). Then a `NoMDEntries` repeating group: a 3-byte dimension
//! (`u16 blockLength + u8 numInGroup`) followed by `numInGroup` entries, each
//! `blockLength` bytes (32) wide.

use zerocopy::little_endian::{I32, I64, U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::enums::{MatchEventIndicator, MdEntryType, MdUpdateAction};
use super::header::SbeHeader;

/// `MDIncrementalRefreshBook` (current SBE variant).
pub const TEMPLATE_INCREMENTAL_BOOK_46: u16 = 46;
/// `MDIncrementalRefreshBook` (legacy SBE variant — same field layout we use).
pub const TEMPLATE_INCREMENTAL_BOOK_32: u16 = 32;

/// Width of one `NoMDEntries` entry on the wire (group `blockLength`).
pub const MD_ENTRY_BLOCK_LEN: u16 = 32;
/// Root `blockLength` for the incremental-refresh message.
pub const INCREMENTAL_ROOT_BLOCK_LEN: u16 = 11;

/// One decoded market-data entry from a `NoMDEntries` group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MdEntry {
    pub security_id: i32,
    pub rpt_seq: u32,
    /// `MDEntryPx` as a raw `PRICE9` integer (price * 1e9).
    pub px_raw: i64,
    pub size: i32,
    pub num_orders: i32,
    /// 1 = top of book.
    pub price_level: u8,
    pub update_action: MdUpdateAction,
    pub entry_type: MdEntryType,
}

/// Fixed fields of one `NoMDEntries` entry (offsets 0..27). The on-wire stride
/// is the group `blockLength` (32), so 5 trailing bytes are skipped per entry.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub(crate) struct MdEntryRaw {
    md_entry_px: I64,
    md_entry_size: I32,
    security_id: I32,
    rpt_seq: U32,
    number_of_orders: I32,
    md_price_level: u8,
    md_update_action: u8,
    md_entry_type: u8,
}

/// Root block of an incremental-refresh message (first 9 bytes; the message's
/// `blockLength` covers the 2 trailing pad bytes).
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct IncrementalRoot {
    transact_time: U64,
    match_event: u8,
}

/// 3-byte repeating-group dimension.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
struct GroupDimension {
    block_length: U16,
    num_in_group: u8,
}

/// A decoded `MDIncrementalRefreshBook` message: its root fields plus a
/// zero-copy view over the entry bytes.
pub struct IncrementalRefresh<'a> {
    transact_time: u64,
    match_event: MatchEventIndicator,
    entries_raw: &'a [u8],
    stride: usize,
    num_in_group: usize,
}

impl<'a> IncrementalRefresh<'a> {
    /// Decode the message `body` (root + groups) given its SBE `header`.
    /// Returns `None` if the template is not an incremental book or the bytes
    /// are truncated/malformed.
    pub fn decode(header: &SbeHeader, body: &'a [u8]) -> Option<IncrementalRefresh<'a>> {
        match header.template_id() {
            TEMPLATE_INCREMENTAL_BOOK_46 | TEMPLATE_INCREMENTAL_BOOK_32 => {}
            _ => return None,
        }

        // Root block is `blockLength` bytes wide (read from the wire so a future
        // schema that grows the root stays parseable).
        let root_len = header.block_length() as usize;
        if body.len() < root_len {
            return None;
        }
        let (root, _) = IncrementalRoot::ref_from_prefix(body).ok()?;
        let after_root = &body[root_len..];

        let (dim, rest) = GroupDimension::ref_from_prefix(after_root).ok()?;
        let stride = dim.block_length.get() as usize;
        let num_in_group = dim.num_in_group as usize;
        if stride == 0 {
            return None;
        }
        let needed = stride.checked_mul(num_in_group)?;
        if rest.len() < needed {
            return None;
        }

        Some(IncrementalRefresh {
            transact_time: root.transact_time.get(),
            match_event: MatchEventIndicator(root.match_event),
            entries_raw: &rest[..needed],
            stride,
            num_in_group,
        })
    }

    /// Transaction time, nanoseconds since the Unix epoch.
    #[inline]
    pub fn transact_time(&self) -> u64 {
        self.transact_time
    }

    /// The match-event indicator (carries the end-of-event bit).
    #[inline]
    pub fn match_event(&self) -> MatchEventIndicator {
        self.match_event
    }

    /// Iterate the decoded entries, skipping any with unknown action/type.
    #[inline]
    pub fn entries(&self) -> MdEntryIter<'a> {
        MdEntryIter {
            raw: self.entries_raw,
            stride: self.stride,
            remaining: self.num_in_group,
        }
    }
}

/// Iterator over decoded `MdEntry` values within a refresh message.
pub struct MdEntryIter<'a> {
    raw: &'a [u8],
    stride: usize,
    remaining: usize,
}

impl<'a> Iterator for MdEntryIter<'a> {
    type Item = MdEntry;

    #[inline]
    fn next(&mut self) -> Option<MdEntry> {
        while self.remaining > 0 {
            let chunk = &self.raw[..self.stride];
            self.raw = &self.raw[self.stride..];
            self.remaining -= 1;

            let (e, _) = MdEntryRaw::ref_from_prefix(chunk).ok()?;
            match (
                MdUpdateAction::from_u8(e.md_update_action),
                MdEntryType::from_byte(e.md_entry_type),
            ) {
                (Some(update_action), Some(entry_type)) => {
                    return Some(MdEntry {
                        security_id: e.security_id.get(),
                        rpt_seq: e.rpt_seq.get(),
                        px_raw: e.md_entry_px.get(),
                        size: e.md_entry_size.get(),
                        num_orders: e.number_of_orders.get(),
                        price_level: e.md_price_level,
                        update_action,
                        entry_type,
                    });
                }
                // Unknown action/type (e.g. an implied/reset variant we don't
                // model here): skip and continue to the next entry.
                _ => continue,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdp3::header::{messages, CME_SCHEMA_ID, SBE_HEADER_LEN};
    use crate::mdp3::packet::PacketHeader;

    fn entry_bytes(e: &MdEntry) -> Vec<u8> {
        let mut b = Vec::with_capacity(MD_ENTRY_BLOCK_LEN as usize);
        b.extend_from_slice(&e.px_raw.to_le_bytes());
        b.extend_from_slice(&e.size.to_le_bytes());
        b.extend_from_slice(&e.security_id.to_le_bytes());
        b.extend_from_slice(&e.rpt_seq.to_le_bytes());
        b.extend_from_slice(&e.num_orders.to_le_bytes());
        b.push(e.price_level);
        b.push(e.update_action as u8);
        b.push(e.entry_type.as_byte());
        b.resize(MD_ENTRY_BLOCK_LEN as usize, 0); // 5 pad bytes
        b
    }

    /// Build the message body (root + group) for an incremental refresh.
    fn refresh_body(transact_time: u64, match_event: u8, entries: &[MdEntry]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&transact_time.to_le_bytes());
        body.push(match_event);
        body.extend_from_slice(&[0u8; 2]); // pad to blockLength 11
        body.extend_from_slice(&MD_ENTRY_BLOCK_LEN.to_le_bytes());
        body.push(entries.len() as u8);
        for e in entries {
            body.extend_from_slice(&entry_bytes(e));
        }
        body
    }

    fn frame(template_id: u16, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let size = (SBE_HEADER_LEN + body.len()) as u16;
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&INCREMENTAL_ROOT_BLOCK_LEN.to_le_bytes());
        out.extend_from_slice(&template_id.to_le_bytes());
        out.extend_from_slice(&CME_SCHEMA_ID.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    fn sample_entries() -> Vec<MdEntry> {
        vec![
            MdEntry {
                security_id: 42,
                rpt_seq: 100,
                px_raw: 5_000_250_000_000, // 5000.25 in PRICE9
                size: 7,
                num_orders: 3,
                price_level: 1,
                update_action: MdUpdateAction::New,
                entry_type: MdEntryType::Bid,
            },
            MdEntry {
                security_id: 42,
                rpt_seq: 101,
                px_raw: 5_000_500_000_000, // 5000.50
                size: 4,
                num_orders: 2,
                price_level: 1,
                update_action: MdUpdateAction::Change,
                entry_type: MdEntryType::Offer,
            },
        ]
    }

    #[test]
    fn decodes_root_and_all_entries() {
        let entries = sample_entries();
        let body = refresh_body(1_700_000_000_000_000_000, 0x80, &entries);

        let hdr = SbeHeader::new(INCREMENTAL_ROOT_BLOCK_LEN, TEMPLATE_INCREMENTAL_BOOK_46, CME_SCHEMA_ID, 0);
        let refresh = IncrementalRefresh::decode(&hdr, &body).expect("decode");

        assert_eq!(refresh.transact_time(), 1_700_000_000_000_000_000);
        assert!(refresh.match_event().is_end_of_event());

        let decoded: Vec<_> = refresh.entries().collect();
        assert_eq!(decoded, entries);
    }

    #[test]
    fn decodes_through_full_packet_framing() {
        let entries = sample_entries();
        let body = refresh_body(123, 0x80, &entries);
        let mut packet = Vec::new();
        packet.extend_from_slice(&9u32.to_le_bytes()); // seq
        packet.extend_from_slice(&456u64.to_le_bytes()); // sending time
        packet.extend(frame(TEMPLATE_INCREMENTAL_BOOK_46, &body));

        let (ph, msg_bytes) = PacketHeader::parse(&packet).unwrap();
        assert_eq!(ph.seq_num(), 9);

        let mut total = 0usize;
        for m in messages(msg_bytes) {
            let r = IncrementalRefresh::decode(m.header, m.body).expect("decode");
            total += r.entries().count();
        }
        assert_eq!(total, 2);
    }

    #[test]
    fn skips_entries_with_unknown_action_or_type() {
        // One valid entry, one with a bogus update action byte (9).
        let valid = sample_entries()[0];
        let mut body = Vec::new();
        body.extend_from_slice(&0u64.to_le_bytes());
        body.push(0x80);
        body.extend_from_slice(&[0u8; 2]);
        body.extend_from_slice(&MD_ENTRY_BLOCK_LEN.to_le_bytes());
        body.push(2);
        body.extend_from_slice(&entry_bytes(&valid));
        let mut bogus = entry_bytes(&valid);
        bogus[25] = 9; // invalid MDUpdateAction
        body.extend_from_slice(&bogus);

        let hdr = SbeHeader::new(INCREMENTAL_ROOT_BLOCK_LEN, TEMPLATE_INCREMENTAL_BOOK_46, CME_SCHEMA_ID, 0);
        let refresh = IncrementalRefresh::decode(&hdr, &body).unwrap();
        let decoded: Vec<_> = refresh.entries().collect();
        assert_eq!(decoded, vec![valid]);
    }

    #[test]
    fn rejects_truncated_group() {
        // numInGroup says 2 but only 1 entry of bytes follows.
        let valid = sample_entries()[0];
        let mut body = Vec::new();
        body.extend_from_slice(&0u64.to_le_bytes());
        body.push(0x80);
        body.extend_from_slice(&[0u8; 2]);
        body.extend_from_slice(&MD_ENTRY_BLOCK_LEN.to_le_bytes());
        body.push(2);
        body.extend_from_slice(&entry_bytes(&valid));

        let hdr = SbeHeader::new(INCREMENTAL_ROOT_BLOCK_LEN, TEMPLATE_INCREMENTAL_BOOK_46, CME_SCHEMA_ID, 0);
        assert!(IncrementalRefresh::decode(&hdr, &body).is_none());
    }
}
