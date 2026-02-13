use bytes::Bytes;
use helix_types::BlsPublicKeyBytes;
use serde::{Deserialize, Serialize};
use ssz::{Decode, DecodeError};
use ssz_derive::{Decode, Encode};

use crate::{
    api::builder::error::BuilderApiError,
    auctioneer::{Compression, MergeType},
};

#[repr(C)]
#[derive(Debug, Clone, Decode)]
pub struct RegistrationMsg {
    pub api_key: [u8; 16], // this is a Uuid
    pub builder_pubkey: BlsPublicKeyBytes,
}

#[derive(Debug, thiserror::Error)]
pub enum BidSubmissionError {
    #[error("Internal error")]
    InternalError,
    #[error(transparent)]
    BuilderApiError(#[from] BuilderApiError),
    #[error("InvalidMergeType {0}")]
    InvalidMergeType(u8),
    #[error("SubmissionTooShort")]
    SubmissionTooShort,
}

#[derive(Debug, Clone)]
pub struct BidSubmission {
    pub header: BidSubmissionHeader,
    pub data: bytes::Bytes,
}

impl TryFrom<&[u8]> for BidSubmission {
    type Error = BidSubmissionError;

    fn try_from(data: &[u8]) -> Result<Self, Self::Error> {
        let header = BidSubmissionHeader::try_from(&data[..BID_SUB_HEADER_SIZE])?;

        tracing::trace!("{:?}", header);

        let bid_submission_data = Bytes::copy_from_slice(&data[BID_SUB_HEADER_SIZE..]);
        Ok(BidSubmission { header, data: bid_submission_data })
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BidSubmissionHeader {
    pub sequence_number: u64,
    pub merge_type: MergeType,
    pub flags: BidSubmissionFlags,
}

const BID_SUB_HEADER_SIZE: usize = 10;

impl TryFrom<&[u8]> for BidSubmissionHeader {
    type Error = BidSubmissionError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        if value.len() < BID_SUB_HEADER_SIZE {
            return Err(BidSubmissionError::SubmissionTooShort)
        }

        let sequence_number = u64::from_be_bytes(value[..8].try_into().unwrap());
        let merge_type = MergeType::try_from(value[8])
            .map_err(|_| BidSubmissionError::InvalidMergeType(value[8]))?;
        let flags = BidSubmissionFlags::from_bits_retain(value[9]);

        Ok(Self { sequence_number, merge_type, flags })
    }
}

impl BidSubmissionHeader {
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

impl Decode for BidSubmissionFlags {
    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() != 1 {
            return Err(DecodeError::InvalidByteLength { len: bytes.len(), expected: 1 });
        }
        Ok(BidSubmissionFlags(bytes[0]))
    }

    fn is_ssz_fixed_len() -> bool {
        true
    }

    fn ssz_fixed_len() -> usize {
        1
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, Encode)]
#[ssz(enum_behaviour = "tag")]
pub enum Status {
    Okay = 0,
    InvalidRequest = 1,
    InternalError = 2,
    // client-only
    // ConnectionError = 255,
}

impl From<&BuilderApiError> for Status {
    fn from(e: &BuilderApiError) -> Self {
        match e {
            BuilderApiError::DatabaseError(_) | BuilderApiError::InternalError => {
                Status::InternalError
            }
            _ => Status::InvalidRequest,
        }
    }
}

impl From<&BidSubmissionError> for Status {
    fn from(e: &BidSubmissionError) -> Self {
        match e {
            BidSubmissionError::InternalError => Status::InternalError,
            BidSubmissionError::BuilderApiError(e) => Status::from(e),
            BidSubmissionError::InvalidMergeType(_) => Status::InvalidRequest,
            BidSubmissionError::SubmissionTooShort => Status::InvalidRequest,
            // _ => Status::InvalidRequest,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Encode)]
pub struct BidSubmissionResponse {
    pub error_msg: Vec<u8>,
    pub sequence_number: u64,
    pub status: Status,
}

impl BidSubmissionResponse {
    pub fn from_builder_api_error(request_id: u64, result: &Result<(), BuilderApiError>) -> Self {
        match result {
            Ok(()) => Self {
                sequence_number: request_id,
                status: Status::Okay,
                error_msg: Default::default(),
            },
            Err(e) => Self {
                sequence_number: request_id,
                status: Status::from(e),
                error_msg: e.to_string().into_bytes(),
            },
        }
    }

    pub fn from_bid_submission_error(request_id: &Option<u64>, e: &BidSubmissionError) -> Self {
        Self {
            sequence_number: request_id.unwrap_or_default(),
            status: Status::from(e),
            error_msg: e.to_string().into_bytes(),
        }
    }
}
