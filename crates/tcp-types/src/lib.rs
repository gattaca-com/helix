use std::fmt::{self, Display};

use bytes::Bytes;
use flux::utils::ArrayVec;
use flux_type_hash_derive::TypeHash;
use serde::{Deserialize, Serialize};
use strum::{AsRefStr, EnumString};

pub const PROTOCOL_VERSION: u8 = 1;
pub const MAX_PUBKEYS: usize = 4;

#[repr(u8)]
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    EnumString,
    AsRefStr,
    Serialize,
    Deserialize,
    ssz_derive::Encode,
    ssz_derive::Decode,
)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
#[ssz(enum_behaviour = "tag")]
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
    Debug,
    Eq,
    PartialEq,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    Hash,
    PartialOrd,
    Ord,
    Default,
    ssz_derive::Encode,
    ssz_derive::Decode,
)]
#[ssz(enum_behaviour = "tag")]
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

/// First message on a new TCP connection. Manually encoded as:
/// `[1B version][16B api_key][1B num_pubkeys][num_pubkeys * 48B pubkeys]`
///
/// Server validates all (api_key, pubkey) pairs against the key cache; on failure drops the
/// connection.
#[derive(Debug, Clone)]
pub struct RegistrationMsg {
    pub version: u8,
    pub api_key: [u8; 16],
    pub builder_pubkeys: ArrayVec<alloy_rpc_types::beacon::BlsPublicKey, MAX_PUBKEYS>,
}

const PUBKEY_SIZE: usize = 48;
const REG_HEADER_SIZE: usize = 1 + 16 + 1; // version + api_key + num_pubkeys

impl TryFrom<&[u8]> for RegistrationMsg {
    type Error = ParseError;

    fn try_from(data: &[u8]) -> Result<Self, Self::Error> {
        if data.len() < REG_HEADER_SIZE {
            return Err(ParseError::RegistrationTooShort);
        }

        let version = data[0];
        match version {
            1 => {}
            v => return Err(ParseError::UnsupportedVersion(v)),
        }

        let mut api_key = [0u8; 16];
        api_key.copy_from_slice(&data[1..17]);

        let num_pubkeys = data[17];
        if num_pubkeys == 0 || num_pubkeys as usize > MAX_PUBKEYS {
            return Err(ParseError::InvalidPubkeyCount(num_pubkeys));
        }

        let expected_len = REG_HEADER_SIZE + num_pubkeys as usize * PUBKEY_SIZE;
        if data.len() < expected_len {
            return Err(ParseError::RegistrationTooShort);
        }

        let mut builder_pubkeys = ArrayVec::new();
        for i in 0..num_pubkeys as usize {
            let offset = REG_HEADER_SIZE + i * PUBKEY_SIZE;
            let pk = alloy_rpc_types::beacon::BlsPublicKey::from_slice(
                &data[offset..offset + PUBKEY_SIZE],
            );
            builder_pubkeys.push(pk);
        }

        Ok(Self { version, api_key, builder_pubkeys })
    }
}

impl RegistrationMsg {
    pub const fn encoded_len(&self) -> usize {
        REG_HEADER_SIZE + self.builder_pubkeys.len() * PUBKEY_SIZE
    }

    /// Encode into `buf`. Panics if `buf.len() < self.encoded_len()`.
    pub fn encode(&self, buf: &mut [u8]) {
        buf[0] = self.version;
        buf[1..17].copy_from_slice(&self.api_key);
        buf[17] = self.builder_pubkeys.len() as u8;
        for (i, pk) in self.builder_pubkeys.iter().enumerate() {
            let offset = REG_HEADER_SIZE + i * PUBKEY_SIZE;
            buf[offset..offset + PUBKEY_SIZE].copy_from_slice(pk.as_ref());
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("InvalidMergeType {0}")]
    InvalidMergeType(u8),
    #[error("SubmissionTooShort")]
    SubmissionTooShort,
    #[error("DuplicateSequenceNumber {0}")]
    DuplicateSequenceNumber(u32),
    #[error("UnsupportedVersion {0}")]
    UnsupportedVersion(u8),
    #[error("InvalidPubkeyCount {0}")]
    InvalidPubkeyCount(u8),
    #[error("RegistrationTooShort")]
    RegistrationTooShort,
}

#[derive(Debug, Clone)]
pub struct BidSubmission {
    pub header: BidSubmissionHeader,
    pub data: Bytes,
}

pub const BID_SUB_HEADER_SIZE: usize = 6;

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
        if value.len() < BID_SUB_HEADER_SIZE {
            return Err(ParseError::SubmissionTooShort)
        }
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

impl Display for BidSubmissionResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let request_id = uuid::Uuid::from_bytes(self.request_id);
        let error_msg = std::str::from_utf8(&self.error_msg).unwrap_or("<invalid utf8>");
        write!(
            f,
            "BidSubmissionResponse {{ sequence_number: {}, request_id: \"{}\", status: {:?}, error_msg: \"{}\" }}",
            self.sequence_number, request_id, self.status, error_msg
        )
    }
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

    #[test]
    fn test_registration_msg_roundtrip() {
        let pk1 = alloy_rpc_types::beacon::BlsPublicKey::from([0xAA; 48]);
        let pk2 = alloy_rpc_types::beacon::BlsPublicKey::from([0xBB; 48]);

        let msg = RegistrationMsg {
            version: PROTOCOL_VERSION,
            api_key: [1u8; 16],
            builder_pubkeys: {
                let mut v = ArrayVec::new();
                v.push(pk1);
                v.push(pk2);
                v
            },
        };

        let mut buf = vec![0u8; msg.encoded_len()];
        msg.encode(&mut buf);

        assert_eq!(buf.len(), REG_HEADER_SIZE + 2 * PUBKEY_SIZE);

        let decoded = RegistrationMsg::try_from(buf.as_slice()).expect("failed to decode");
        assert_eq!(decoded.version, PROTOCOL_VERSION);
        assert_eq!(decoded.api_key, [1u8; 16]);
        assert_eq!(decoded.builder_pubkeys.len(), 2);
        assert_eq!(decoded.builder_pubkeys[0], pk1);
        assert_eq!(decoded.builder_pubkeys[1], pk2);
    }
}
