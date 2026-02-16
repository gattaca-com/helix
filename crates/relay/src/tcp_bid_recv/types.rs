use bytes::Bytes;
use helix_types::{BlsPublicKeyBytes, SeqNum};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};

use crate::{
    api::builder::error::BuilderApiError,
    auctioneer::{Compression, MergeType},
};

/// First message on a new TCP connection. SSZ-encoded.
/// Server validates the (api_key, builder_pubkey) pair against the key cache; on failure drops the
/// connection.
#[repr(C)]
#[derive(Debug, Clone, Decode)]
pub struct RegistrationMsg {
    pub api_key: [u8; 16], // UUID bytes
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
        if data.len() < BID_SUB_HEADER_SIZE {
            return Err(BidSubmissionError::SubmissionTooShort)
        }

        let header = BidSubmissionHeader::try_from(&data[..BID_SUB_HEADER_SIZE])?;

        tracing::trace!("{:?}", header);

        let bid_submission_data = Bytes::copy_from_slice(&data[BID_SUB_HEADER_SIZE..]);
        Ok(BidSubmission { header, data: bid_submission_data })
    }
}

/// Wire header for bid submission. Manually encoded as
/// `[16B BE seq_number][1B merge_type][1B flags]` (18 bytes total).
/// The sequence_number is used as a unique identifier of the submission on both sides.
/// Unlike HTTP the api key is not sent, because it is validated on receipt of the registration
/// message.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BidSubmissionHeader {
    pub sequence_number: SeqNum,
    pub merge_type: MergeType,
    pub flags: BidSubmissionFlags,
}

const BID_SUB_HEADER_SIZE: usize = 18;

impl TryFrom<&[u8]> for BidSubmissionHeader {
    type Error = BidSubmissionError;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        let sequence_number = u128::from_be_bytes(value[..16].try_into().unwrap());
        let merge_type = MergeType::try_from(value[16])
            .map_err(|_| BidSubmissionError::InvalidMergeType(value[16]))?;
        let flags = BidSubmissionFlags::from_bits_retain(value[17]);

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

/// SSZ-encoded server response sent back in a single frame.
#[repr(C)]
#[derive(Debug, Clone, Encode)]
pub struct BidSubmissionResponse {
    pub sequence_number: SeqNum,
    pub status: Status,
    pub error_msg: Vec<u8>,
}

impl BidSubmissionResponse {
    pub fn from_builder_api_error(
        request_id: SeqNum,
        result: &Result<(), BuilderApiError>,
    ) -> Self {
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

    pub fn from_bid_submission_error(request_id: &Option<SeqNum>, e: &BidSubmissionError) -> Self {
        Self {
            sequence_number: request_id.unwrap_or_default(),
            status: Status::from(e),
            error_msg: e.to_string().into_bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        auctioneer::MergeType,
        tcp_bid_recv::{
            BidSubmissionHeader,
            types::{BID_SUB_HEADER_SIZE, BidSubmissionFlags},
        },
    };

    #[test]
    fn test_bid_submission_header() {
        let header = BidSubmissionHeader {
            sequence_number: 1913u128,
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
