//! CME Market Data Platform 3.0 (MDP 3.0) wire codec.
//!
//! Layout of one UDP payload (all little-endian):
//!
//! ```text
//! +-----------------------------+  CME binary packet header (12 bytes)
//! | MsgSeqNum     u32  @0        |
//! | SendingTime   u64  @4 (ns)  |
//! +-----------------------------+  then 1..N SBE messages, each:
//! | MessageSize   u16           |  size of the SBE message (incl. its 8-byte header)
//! | --- SBE message header (8) -|
//! | blockLength   u16           |
//! | templateId    u16           |
//! | schemaId      u16           |
//! | version       u16           |
//! | --- root block ------------ |
//! | --- repeating groups ------ |
//! +-----------------------------+
//! ```
//!
//! Fields are packed and therefore unaligned within the buffer, so every
//! multi-byte field uses `zerocopy::little_endian` wrapper types (alignment 1)
//! and the structs derive `Unaligned` — reading them as native `u64`/`i64`
//! would be undefined behaviour.

pub mod book_refresh;
pub mod encode;
pub mod enums;
pub mod header;
pub mod packet;
