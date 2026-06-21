//! Wire-faithful MDP 3.0 packet encoder.
//!
//! The generator uses exactly this code to synthesize packets, so the encoder
//! and decoder share one definition of the wire format and cannot drift.

use zerocopy::IntoBytes;

use super::book_refresh::{
    MdEntry, INCREMENTAL_ROOT_BLOCK_LEN, MD_ENTRY_BLOCK_LEN, TEMPLATE_INCREMENTAL_BOOK_46,
};
use super::header::{SbeHeader, CME_SCHEMA_ID, SBE_HEADER_LEN};

/// Accumulates a single UDP payload: the 12-byte packet header followed by one
/// or more framed SBE messages.
pub struct PacketBuilder {
    buf: Vec<u8>,
}

impl PacketBuilder {
    /// Start a packet with the given sequence number and sending time (ns).
    pub fn new(seq: u32, sending_time: u64) -> PacketBuilder {
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(&seq.to_le_bytes());
        buf.extend_from_slice(&sending_time.to_le_bytes());
        PacketBuilder { buf }
    }

    /// Append an `MDIncrementalRefreshBook` (template 46) message.
    pub fn add_incremental_refresh(
        &mut self,
        transact_time: u64,
        match_event: u8,
        entries: &[MdEntry],
    ) -> &mut PacketBuilder {
        // Build the SBE message body: root block + group dimension + entries.
        let mut body = Vec::with_capacity(
            INCREMENTAL_ROOT_BLOCK_LEN as usize
                + 3
                + entries.len() * MD_ENTRY_BLOCK_LEN as usize,
        );
        body.extend_from_slice(&transact_time.to_le_bytes());
        body.push(match_event);
        body.extend_from_slice(&[0u8; 2]); // pad to root blockLength (11)
        body.extend_from_slice(&MD_ENTRY_BLOCK_LEN.to_le_bytes());
        body.push(entries.len() as u8);
        for e in entries {
            encode_entry(&mut body, e);
        }

        // Frame: u16 MessageSize | SBE header | body.
        let sbe = SbeHeader::new(
            INCREMENTAL_ROOT_BLOCK_LEN,
            TEMPLATE_INCREMENTAL_BOOK_46,
            CME_SCHEMA_ID,
            0,
        );
        let size = (SBE_HEADER_LEN + body.len()) as u16;
        self.buf.extend_from_slice(&size.to_le_bytes());
        self.buf.extend_from_slice(sbe.as_bytes());
        self.buf.extend_from_slice(&body);
        self
    }

    /// Number of messages appended so far is not tracked; finish consumes the
    /// builder and yields the full packet bytes.
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    /// Borrow the bytes accumulated so far.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }
}

/// Encode one `NoMDEntries` entry (27 field bytes + 5 trailing pad = 32).
fn encode_entry(out: &mut Vec<u8>, e: &MdEntry) {
    let start = out.len();
    out.extend_from_slice(&e.px_raw.to_le_bytes());
    out.extend_from_slice(&e.size.to_le_bytes());
    out.extend_from_slice(&e.security_id.to_le_bytes());
    out.extend_from_slice(&e.rpt_seq.to_le_bytes());
    out.extend_from_slice(&e.num_orders.to_le_bytes());
    out.push(e.price_level);
    out.push(e.update_action as u8);
    out.push(e.entry_type.as_byte());
    out.resize(start + MD_ENTRY_BLOCK_LEN as usize, 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdp3::book_refresh::IncrementalRefresh;
    use crate::mdp3::enums::{MdEntryType, MdUpdateAction};
    use crate::mdp3::header::messages;
    use crate::mdp3::packet::PacketHeader;

    fn entries() -> Vec<MdEntry> {
        vec![
            MdEntry {
                security_id: 5,
                rpt_seq: 1,
                px_raw: 4_200_750_000_000,
                size: 12,
                num_orders: 4,
                price_level: 1,
                update_action: MdUpdateAction::New,
                entry_type: MdEntryType::Bid,
            },
            MdEntry {
                security_id: 5,
                rpt_seq: 2,
                px_raw: 4_201_000_000_000,
                size: 9,
                num_orders: 1,
                price_level: 1,
                update_action: MdUpdateAction::New,
                entry_type: MdEntryType::Offer,
            },
        ]
    }

    fn decode_all(packet: &[u8]) -> (u32, u64, Vec<MdEntry>) {
        let (ph, body) = PacketHeader::parse(packet).expect("packet header");
        let mut all = Vec::new();
        for m in messages(body) {
            let r = IncrementalRefresh::decode(m.header, m.body).expect("decode");
            all.extend(r.entries());
        }
        (ph.seq_num(), ph.sending_time(), all)
    }

    #[test]
    fn round_trips_a_single_refresh() {
        let entries = entries();
        let mut b = PacketBuilder::new(11, 22);
        b.add_incremental_refresh(123, 0x80, &entries);
        let packet = b.finish();

        let (seq, ts, decoded) = decode_all(&packet);
        assert_eq!(seq, 11);
        assert_eq!(ts, 22);
        assert_eq!(decoded, entries);
    }

    #[test]
    fn round_trips_multiple_messages_in_one_packet() {
        let entries = entries();
        let mut b = PacketBuilder::new(7, 0);
        b.add_incremental_refresh(1, 0x00, &entries[..1]);
        b.add_incremental_refresh(2, 0x80, &entries[1..]);
        let packet = b.finish();

        let (_, _, decoded) = decode_all(&packet);
        assert_eq!(decoded, entries);
    }
}
