use std::fmt::{self, Display};

use bytes::Bytes;
use flux_type_hash_derive::TypeHash;
use serde::{Deserialize, Serialize};
use strum::{AsRefStr, EnumString};

#[repr(u8)]
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, EnumString, AsRefStr, Serialize, Deserialize,
)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum MergeType {
    #[default]
    None = 0,
    Mergeable = 1,
    AppendOnly = 2,
}

impl MergeType {
    pub fn is_some(&self) -> bool {
        *self != Self::None
    }
}

impl TryFrom<u8> for MergeType {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Mergeable),
            2 => Ok(Self::AppendOnly),
            _ => Err(()),
        }
    }
}

#[repr(u8)]
#[derive(
    Debug, Eq, PartialEq, Clone, Copy, Serialize, Deserialize, Hash, PartialOrd, Ord, Default,
)]
pub enum Compression {
    #[default]
    None = 0,
    Gzip = 1,
    Zstd = 2,
}

impl Compression {
    pub fn string(&self) -> String {
        match self {
            Compression::None => "none".into(),
            Compression::Gzip => "gzip".into(),
            Compression::Zstd => "zstd".into(),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Compression::None => "none",
            Compression::Gzip => "gzip",
            Compression::Zstd => "zstd",
        }
    }
}

impl std::fmt::Display for Compression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            Compression::None => "NONE",
            Compression::Gzip => "GZIP",
            Compression::Zstd => "ZSTD",
        })
    }
}

/// First message on a new TCP connection. SSZ-encoded.
/// Server validates the (api_key, builder_pubkey) pair against the key cache; on failure drops the
/// connection.
#[repr(C)]
#[derive(Debug, Clone, ssz_derive::Decode, ssz_derive::Encode)]
pub struct RegistrationMsg {
    pub api_key: [u8; 16], // UUID bytes
    pub builder_pubkey: alloy_rpc_types::beacon::BlsPublicKey,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("InvalidMergeType {0}")]
    InvalidMergeType(u8),
    #[error("SubmissionTooShort")]
    SubmissionTooShort,
    #[error("DuplicateSequenceNumber {0}")]
    DuplicateSequenceNumber(u32),
}

#[derive(Debug, Clone)]
pub struct BidSubmission {
    pub header: BidSubmissionHeader,
    pub data: Bytes,
}

const BID_SUB_HEADER_SIZE: usize = 6;

impl TryFrom<&[u8]> for BidSubmission {
    type Error = ParseError;

    fn try_from(data: &[u8]) -> Result<Self, Self::Error> {
        if data.len() < BID_SUB_HEADER_SIZE {
            return Err(ParseError::SubmissionTooShort)
        }

        let header = BidSubmissionHeader::try_from(&data[..BID_SUB_HEADER_SIZE])?;

        tracing::trace!("{:?}", header);

        let data = Bytes::copy_from_slice(&data[BID_SUB_HEADER_SIZE..]);
        Ok(BidSubmission { header, data })
    }
}

/// Wire header for bid submission. Manually encoded as
/// `[4B BE seq_number][1B merge_type][1B flags]` (6 bytes total).
/// The sequence_number is used as a unique identifier of the submission on the client side per
/// slot. Unlike HTTP the api key is not sent, because it is validated on receipt of the
/// registration message.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BidSubmissionHeader {
    pub sequence_number: u32,
    pub merge_type: MergeType,
    pub flags: BidSubmissionFlags,
}

impl TryFrom<&[u8]> for BidSubmissionHeader {
    type Error = ParseError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        let sequence_number = u32::from_be_bytes(value[..4].try_into().unwrap());
        let merge_type =
            MergeType::try_from(value[4]).map_err(|_| ParseError::InvalidMergeType(value[4]))?;
        let flags = BidSubmissionFlags::from_bits_retain(value[5]);

        Ok(Self { sequence_number, merge_type, flags })
    }
}

impl BidSubmissionHeader {
    pub fn append_encoded(self, buffer: &mut Vec<u8>) {
        buffer.extend_from_slice(&self.sequence_number.to_be_bytes());
        buffer.push(self.merge_type as u8);
        buffer.push(self.flags.bits());
    }

    pub fn compression(&self) -> Compression {
        if self.flags.is_zstd_compressed() { Compression::Zstd } else { Compression::None }
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct BidSubmissionFlags(u8);

bitflags::bitflags! {
    impl BidSubmissionFlags : u8 {
        const IS_DEHYDRATED = 1 << 0;
        const WITH_ADJUSTMENTS = 1 << 1;
        const ZSTD_COMPRESSED = 1 << 2;
    }
}

impl BidSubmissionFlags {
    pub fn is_dehydrated(&self) -> bool {
        self.contains(Self::IS_DEHYDRATED)
    }
    pub fn with_adjustments(&self) -> bool {
        self.contains(Self::WITH_ADJUSTMENTS)
    }
    pub fn is_zstd_compressed(&self) -> bool {
        self.contains(Self::ZSTD_COMPRESSED)
    }
}

#[repr(u8)]
#[derive(
    Default,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Deserialize,
    Serialize,
    Hash,
    TypeHash,
    ssz_derive::Encode,
    ssz_derive::Decode,
)]
#[ssz(enum_behaviour = "tag")]
pub enum Status {
    #[default]
    Okay = 0,
    InvalidRequest = 1,
    InternalError = 2,
    // client only
    ConnectionError = 255,
}

impl Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt::Debug::fmt(&self, f)
    }
}

impl Status {
    pub fn is_okay(&self) -> bool {
        *self == Status::Okay
    }
    pub fn is_err(&self) -> bool {
        !self.is_okay()
    }
}

/// SSZ-encoded server response sent back in a single frame.
#[repr(C)]
#[derive(Debug, Clone, ssz_derive::Encode, ssz_derive::Decode)]
pub struct BidSubmissionResponse {
    pub sequence_number: u32,
    pub request_id: [u8; 16], // UUID
    pub status: Status,
    pub error_msg: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bid_submission_header() {
        let header = BidSubmissionHeader {
            sequence_number: 1913,
            merge_type: MergeType::Mergeable,
            flags: BidSubmissionFlags::all(),
        };

        let mut buffer = Vec::new();
        header.append_encoded(&mut buffer);

        assert_eq!(buffer.len(), BID_SUB_HEADER_SIZE);

        let decoded =
            BidSubmissionHeader::try_from(buffer.as_slice()).expect("failed to decode header");

        assert_eq!(decoded, header);
    }
}
