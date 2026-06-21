//! CME binary packet header: a 12-byte little-endian prefix on every UDP payload.

use zerocopy::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// Length of the CME binary packet header in bytes.
pub const PACKET_HEADER_LEN: usize = 12;

/// The 12-byte CME binary packet header that prefixes every UDP datagram.
///
/// `MsgSeqNum` is the per-channel+feed packet sequence number (used for
/// feed-level gap detection); `SendingTime` is nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct PacketHeader {
    seq_num: U32,
    sending_time: U64,
}

impl PacketHeader {
    /// Parse the packet header from the front of `buf`, returning the header and
    /// the remaining bytes (the framed SBE messages). Returns `None` if `buf` is
    /// too short to contain a header.
    #[inline]
    pub fn parse(buf: &[u8]) -> Option<(&PacketHeader, &[u8])> {
        PacketHeader::ref_from_prefix(buf).ok()
    }

    /// Per-channel+feed packet sequence number.
    #[inline]
    pub fn seq_num(&self) -> u32 {
        self.seq_num.get()
    }

    /// Sending time, nanoseconds since the Unix epoch.
    #[inline]
    pub fn sending_time(&self) -> u64 {
        self.sending_time.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_bytes(seq: u32, sending_time: u64) -> [u8; PACKET_HEADER_LEN] {
        let mut buf = [0u8; PACKET_HEADER_LEN];
        buf[0..4].copy_from_slice(&seq.to_le_bytes());
        buf[4..12].copy_from_slice(&sending_time.to_le_bytes());
        buf
    }

    #[test]
    fn parses_seq_num_and_sending_time_little_endian() {
        let buf = header_bytes(0x0102_0304, 0x0102_0304_0506_0708);
        let (hdr, body) = PacketHeader::parse(&buf).expect("12 bytes is enough");
        assert_eq!(hdr.seq_num(), 0x0102_0304);
        assert_eq!(hdr.sending_time(), 0x0102_0304_0506_0708);
        assert_eq!(body.len(), 0, "no message bytes follow a bare header");
    }

    #[test]
    fn returns_trailing_message_bytes_as_body() {
        let mut buf = header_bytes(7, 999).to_vec();
        buf.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        let (hdr, body) = PacketHeader::parse(&buf).expect("parse");
        assert_eq!(hdr.seq_num(), 7);
        assert_eq!(body, &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn rejects_buffer_shorter_than_header() {
        assert!(PacketHeader::parse(&[0u8; 8]).is_none());
        assert!(PacketHeader::parse(&[]).is_none());
    }
}
