use crate::Status;

pub const BASE_PREFIX_LEN: usize = 33;

#[derive(Debug, Clone, PartialEq, Eq, ssz_derive::Encode, ssz_derive::Decode)]
pub struct RebidV1 {
    pub version: u8,
    pub slot: u64,
    pub base_id: [u8; 32],
    pub value: [u8; 32],
    pub block_hash: [u8; 32],
    pub state_root: [u8; 32],
    pub signature: [u8; 96],
    pub payment_transaction: Vec<u8>,
    pub adjustment: Vec<u8>,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ssz_derive::Encode, ssz_derive::Decode)]
#[ssz(enum_behaviour = "tag")]
pub enum RebidError {
    #[default]
    None = 0,
    MissingBase = 1,
    InvalidPatch = 2,
    ConflictingBase = 3,
    CacheFull = 4,
}

#[derive(Debug, Clone, ssz_derive::Encode, ssz_derive::Decode)]
pub struct RebidResponse {
    pub sequence_number: u32,
    pub request_id: [u8; 16],
    pub status: Status,
    pub error: RebidError,
    pub base_ready: bool,
    pub error_msg: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use ssz::{Decode, Encode};

    use super::*;

    #[test]
    fn rebid_wire_round_trip_and_version() {
        let bid = RebidV1 {
            version: 1,
            slot: 12,
            base_id: [7; 32],
            value: [8; 32],
            block_hash: [9; 32],
            state_root: [10; 32],
            signature: [11; 96],
            payment_transaction: vec![12; 150],
            adjustment: vec![],
        };
        let bytes = bid.as_ssz_bytes();
        assert_eq!(RebidV1::from_ssz_bytes(&bytes).unwrap(), bid);
        assert_eq!(bytes.len(), 391);
        assert!(RebidV1::from_ssz_bytes(&bytes[..240]).is_err());
    }

    #[test]
    fn rebid_response_preserves_typed_error() {
        let response = RebidResponse {
            sequence_number: 42,
            request_id: [0; 16],
            status: crate::Status::InvalidRequest,
            error: RebidError::MissingBase,
            base_ready: false,
            error_msg: vec![],
        };
        let decoded = RebidResponse::from_ssz_bytes(&response.as_ssz_bytes()).unwrap();
        assert_eq!(decoded.error, RebidError::MissingBase);
        assert_eq!(decoded.sequence_number, 42);
    }
}
