//! Wire types for the builder block-merging TCP protocol.
//!
//! Transport frames are flux `TcpStream` frames (`[u32 LE len][u64 LE send-ts][payload]`).
//! The payload is `[1B msg_type][1B flags][SSZ body]`. The 2-byte header stays outside
//! SSZ so the receiver can route and decompress before decoding.

pub mod builder_to_relay;
pub mod control;
pub mod order;
pub mod relay_to_builder;

use bitflags::bitflags;

pub const MERGING_PROTOCOL_VERSION: u16 = 1;
pub const MERGING_HEADER_SIZE: usize = 2;

/// Message-type IDs. `0x00-0x0f` control, `0x10-0x3f` relay->builder,
/// `0x40-0x6f` builder->relay, `0x70-0x7f` errors. IDs `>= 0x80` are extensions:
/// receivers must ignore unknown IDs there; an unknown ID below `0x80` is a
/// protocol error and the receiver disconnects.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergingMsgId {
    MergerRegistrationV1 = 0x01,
    MergerAckV1 = 0x02,
    PingV1 = 0x03,
    PongV1 = 0x04,
    RelayConfigV1 = 0x05,
    SlotStartV1 = 0x10,
    MergeableBlockV1 = 0x11,
    ActivateBaseBlockV1 = 0x12,
    SlotEndV1 = 0x15,
    MergedBlockV1 = 0x40,
    RejectV1 = 0x70,
    FatalV1 = 0x71,
}

impl TryFrom<u8> for MergingMsgId {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0x01 => Self::MergerRegistrationV1,
            0x02 => Self::MergerAckV1,
            0x03 => Self::PingV1,
            0x04 => Self::PongV1,
            0x05 => Self::RelayConfigV1,
            0x10 => Self::SlotStartV1,
            0x11 => Self::MergeableBlockV1,
            0x12 => Self::ActivateBaseBlockV1,
            0x15 => Self::SlotEndV1,
            0x40 => Self::MergedBlockV1,
            0x70 => Self::RejectV1,
            0x71 => Self::FatalV1,
            other => return Err(other),
        })
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct MergingFlags(u8);

bitflags! {
    impl MergingFlags: u8 {
        const ZSTD_COMPRESSED = 1 << 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MergingFrameHeader {
    pub msg_id: MergingMsgId,
    pub flags: MergingFlags,
}

#[derive(Debug, thiserror::Error)]
pub enum MergingHeaderError {
    #[error("frame too short")]
    TooShort,
    #[error("unknown msg id {0:#04x}")]
    UnknownMsgId(u8),
    /// ID in the extension range: skip the frame, keep the connection.
    #[error("extension msg id {0:#04x}")]
    ExtensionMsgId(u8),
}

impl MergingFrameHeader {
    pub fn new(msg_id: MergingMsgId) -> Self {
        Self { msg_id, flags: MergingFlags::empty() }
    }

    pub fn with_zstd(mut self) -> Self {
        self.flags |= MergingFlags::ZSTD_COMPRESSED;
        self
    }

    pub fn is_zstd_compressed(&self) -> bool {
        self.flags.contains(MergingFlags::ZSTD_COMPRESSED)
    }

    pub fn append_encoded(self, buffer: &mut Vec<u8>) {
        buffer.push(self.msg_id as u8);
        buffer.push(self.flags.bits());
    }

    /// Decodes the header; the body is `payload[MERGING_HEADER_SIZE..]`.
    pub fn decode(payload: &[u8]) -> Result<Self, MergingHeaderError> {
        if payload.len() < MERGING_HEADER_SIZE {
            return Err(MergingHeaderError::TooShort);
        }
        let msg_id = MergingMsgId::try_from(payload[0]).map_err(|id| {
            if id >= 0x80 {
                MergingHeaderError::ExtensionMsgId(id)
            } else {
                MergingHeaderError::UnknownMsgId(id)
            }
        })?;
        Ok(Self { msg_id, flags: MergingFlags::from_bits_retain(payload[1]) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let header = MergingFrameHeader::new(MergingMsgId::MergeableBlockV1).with_zstd();
        let mut buf = Vec::new();
        header.append_encoded(&mut buf);
        assert_eq!(buf.len(), MERGING_HEADER_SIZE);
        assert_eq!(MergingFrameHeader::decode(&buf).unwrap(), header);
    }

    #[test]
    fn header_unknown_vs_extension() {
        assert!(matches!(
            MergingFrameHeader::decode(&[0x7f, 0]),
            Err(MergingHeaderError::UnknownMsgId(0x7f))
        ));
        assert!(matches!(
            MergingFrameHeader::decode(&[0x80, 0]),
            Err(MergingHeaderError::ExtensionMsgId(0x80))
        ));
    }
}
