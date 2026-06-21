//! SBE message header and intra-packet message framing.
//!
//! After the 12-byte packet header, a UDP payload carries one or more SBE
//! messages, each framed as `u16 MessageSize` followed by `MessageSize` bytes
//! that begin with the 8-byte SBE message header.

use zerocopy::little_endian::U16;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// Length of the SBE message header in bytes.
pub const SBE_HEADER_LEN: usize = 8;

/// Length of the `MessageSize` framing prefix in bytes.
pub const MSG_SIZE_PREFIX_LEN: usize = 2;

/// CME MDP 3.0 SBE schema id.
pub const CME_SCHEMA_ID: u16 = 1;

/// The 8-byte SBE message header preceding each message's root block.
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct SbeHeader {
    block_length: U16,
    template_id: U16,
    schema_id: U16,
    version: U16,
}

impl SbeHeader {
    /// Construct an SBE header (used by the encoder and tests).
    #[inline]
    pub fn new(block_length: u16, template_id: u16, schema_id: u16, version: u16) -> SbeHeader {
        SbeHeader {
            block_length: U16::new(block_length),
            template_id: U16::new(template_id),
            schema_id: U16::new(schema_id),
            version: U16::new(version),
        }
    }

    /// Size of the message root block (fixed fields), in bytes.
    #[inline]
    pub fn block_length(&self) -> u16 {
        self.block_length.get()
    }
    /// SBE template id identifying the message type (e.g. 46 = incremental book).
    #[inline]
    pub fn template_id(&self) -> u16 {
        self.template_id.get()
    }
    /// SBE schema id (1 for CME MDP 3.0).
    #[inline]
    pub fn schema_id(&self) -> u16 {
        self.schema_id.get()
    }
    /// Schema version.
    #[inline]
    pub fn version(&self) -> u16 {
        self.version.get()
    }
}

/// One framed SBE message: its header plus the bytes after the header
/// (root block + repeating groups).
#[derive(Debug, Clone, Copy)]
pub struct SbeMessage<'a> {
    pub header: &'a SbeHeader,
    /// Root block + groups (length == `MessageSize` - 8).
    pub body: &'a [u8],
}

/// Iterator over the SBE messages framed within a packet body.
///
/// Stops cleanly (yielding `None`) on truncation rather than panicking, so a
/// malformed trailing fragment never crashes the hot path.
pub struct SbeMessageIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for SbeMessageIter<'a> {
    type Item = SbeMessage<'a>;

    #[inline]
    fn next(&mut self) -> Option<SbeMessage<'a>> {
        if self.remaining.len() < MSG_SIZE_PREFIX_LEN {
            return None;
        }
        let size = u16::from_le_bytes([self.remaining[0], self.remaining[1]]) as usize;
        // A message must at least contain its own 8-byte SBE header.
        if size < SBE_HEADER_LEN {
            return None;
        }
        let total = MSG_SIZE_PREFIX_LEN + size;
        if self.remaining.len() < total {
            return None;
        }
        let msg_bytes = &self.remaining[MSG_SIZE_PREFIX_LEN..total];
        self.remaining = &self.remaining[total..];
        let (header, body) = SbeHeader::ref_from_prefix(msg_bytes).ok()?;
        Some(SbeMessage { header, body })
    }
}

/// Iterate the SBE messages framed in `body` (the bytes after the packet header).
#[inline]
pub fn messages(body: &[u8]) -> SbeMessageIter<'_> {
    SbeMessageIter { remaining: body }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frame one SBE message: `u16 size | SbeHeader | payload`.
    fn frame(template_id: u16, block_length: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let size = (SBE_HEADER_LEN + payload.len()) as u16;
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&block_length.to_le_bytes());
        out.extend_from_slice(&template_id.to_le_bytes());
        out.extend_from_slice(&CME_SCHEMA_ID.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // version
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn iterates_two_framed_messages() {
        let mut body = frame(46, 11, &[1, 2, 3]);
        body.extend(frame(52, 18, &[9, 9]));

        let msgs: Vec<_> = messages(&body).collect();
        assert_eq!(msgs.len(), 2);

        assert_eq!(msgs[0].header.template_id(), 46);
        assert_eq!(msgs[0].header.block_length(), 11);
        assert_eq!(msgs[0].header.schema_id(), CME_SCHEMA_ID);
        assert_eq!(msgs[0].body, &[1, 2, 3]);

        assert_eq!(msgs[1].header.template_id(), 52);
        assert_eq!(msgs[1].body, &[9, 9]);
    }

    #[test]
    fn empty_body_yields_no_messages() {
        assert_eq!(messages(&[]).count(), 0);
    }

    #[test]
    fn stops_on_truncated_size_prefix() {
        // A single stray byte can't start a message.
        assert_eq!(messages(&[0x05]).count(), 0);
    }

    #[test]
    fn stops_when_declared_size_exceeds_remaining() {
        // size says 8 bytes follow but only 4 are present.
        let mut body = Vec::new();
        body.extend_from_slice(&8u16.to_le_bytes());
        body.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(messages(&body).count(), 0);
    }
}
