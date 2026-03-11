use std::sync::Arc;

use alloy_primitives::{B256, Bytes};
use helix_types::{
    BidTrace, BlobsBundle, BlsSignatureBytes, ExecutionPayload, ExecutionRequests,
    SignedBidSubmission,
};
use ssz_derive::{Decode, Encode};
use thiserror::Error;

use crate::{ValidatorPreferences, api::builder_api::InclusionListWithMetadata};

/// Wire format of `signed_bid_submission` in `SimRequest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum SubmissionFormat {
    /// Uncompressed SSZ `SignedBidSubmission` (current default).
    #[default]
    FullSsz = 0,
    /// Uncompressed SSZ `DehydratedBidSubmission`.
    DehydratedSsz = 1,
}

impl ssz::Encode for SubmissionFormat {
    fn is_ssz_fixed_len() -> bool {
        true
    }
    fn ssz_fixed_len() -> usize {
        1
    }
    fn ssz_bytes_len(&self) -> usize {
        1
    }
    fn ssz_append(&self, buf: &mut Vec<u8>) {
        buf.push(*self as u8);
    }
}

impl ssz::Decode for SubmissionFormat {
    fn is_ssz_fixed_len() -> bool {
        true
    }
    fn ssz_fixed_len() -> usize {
        1
    }
    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        match bytes {
            [0] => Ok(Self::FullSsz),
            [1] => Ok(Self::DehydratedSsz),
            _ => {
                Err(ssz::DecodeError::BytesInvalid(format!("unknown SubmissionFormat: {bytes:?}")))
            }
        }
    }
}

const UNKNOWN_ANCESTOR: &str = "unknown ancestor";
const PARENT_NOT_FOUND: &str = "parent block not found";
const MISSING_TRIE_NODE: &str = "missing trie node";
const BLOCK_ALREADY_KNOWN: &str = "block already known";
const BLOCK_TOO_OLD: &str = "block is too old, outside validation window";
const BLOCK_REQ_REORG: &str = "block requires a reorg";
const PARENT_BLOCK_NOT_FOUND: &str = "could not find parent block: parent block not found";

#[derive(Debug, Clone, Error)]
pub enum BlockSimError {
    #[error("block validation failed. Reason: {0}")]
    BlockValidationFailed(String),

    #[error("invalid tx root: got {got}, expected: {expected}")]
    InvalidTxRoot { got: B256, expected: B256 },

    #[error("validation request timeout")]
    Timeout,

    #[error("rpc error")]
    RpcError,

    #[error("tokio::mpsc send error")]
    SendError,

    #[error("no simulator available")]
    NoSimulatorAvailable,

    #[error("simulation dropped")]
    SimulationDropped,

    #[error("hydration miss: simulator cache does not have required transactions/blobs")]
    HydrationMiss,
}

impl BlockSimError {
    pub fn is_temporary(&self) -> bool {
        match self {
            BlockSimError::BlockValidationFailed(reason) => match reason.to_lowercase().as_str() {
                UNKNOWN_ANCESTOR => true,
                PARENT_NOT_FOUND => true,
                PARENT_BLOCK_NOT_FOUND => true,
                BLOCK_REQ_REORG => true,
                r if r.starts_with(MISSING_TRIE_NODE) => true,
                _ => false,
            },
            BlockSimError::Timeout => true,
            BlockSimError::RpcError => true,
            BlockSimError::NoSimulatorAvailable => true,
            _ => false,
        }
    }

    pub fn is_already_known(&self) -> bool {
        match self {
            BlockSimError::BlockValidationFailed(reason) => {
                matches!(reason.to_lowercase().as_str(), BLOCK_ALREADY_KNOWN)
            }
            _ => false,
        }
    }

    pub fn is_too_old(&self) -> bool {
        match self {
            BlockSimError::BlockValidationFailed(reason) => {
                matches!(reason.to_lowercase().as_str(), BLOCK_TOO_OLD)
            }
            _ => false,
        }
    }

    pub fn is_demotable(&self) -> bool {
        !self.is_already_known() && !self.is_temporary() && !self.is_too_old()
    }
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct SszValidationRequest {
    pub apply_blacklist: bool,
    pub registered_gas_limit: u64,
    pub parent_beacon_block_root: B256,
    pub inclusion_list: InclusionListWithMetadata,
    pub format: SubmissionFormat,
    pub signed_bid_submission: Bytes,
}

// TODO: refactor this in a SignedBidSubmission + extra fields
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JsonValidationRequest {
    #[serde(with = "serde_utils::quoted_u64")]
    pub registered_gas_limit: u64,
    pub message: BidTrace,
    pub execution_payload: ExecutionPayload,
    pub signature: BlsSignatureBytes,
    pub proposer_preferences: ValidatorPreferences,
    pub blobs_bundle: Option<Arc<BlobsBundle>>,
    pub execution_requests: Option<Arc<ExecutionRequests>>,
    pub parent_beacon_block_root: Option<B256>,
    pub inclusion_list: Option<InclusionListWithMetadata>,
    pub apply_blacklist: bool,
}

impl JsonValidationRequest {
    pub fn new(
        registered_gas_limit: u64,
        block: &SignedBidSubmission,
        proposer_preferences: ValidatorPreferences,
        parent_beacon_block_root: Option<B256>,
        inclusion_list: Option<InclusionListWithMetadata>,
    ) -> Self {
        Self {
            registered_gas_limit,
            message: block.bid_trace().clone(),
            execution_payload: block.execution_payload_ref().clone(),
            signature: *block.signature(),
            apply_blacklist: proposer_preferences.filtering.is_regional(),
            proposer_preferences,
            blobs_bundle: Some(block.blobs_bundle().clone()),
            execution_requests: Some(block.execution_requests_ref().clone()),
            parent_beacon_block_root,
            inclusion_list,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_temporary() {
        let s = String::from("could not find parent block: parent block not found");
        let err = BlockSimError::BlockValidationFailed(s);

        assert!(err.is_temporary())
    }

    #[test]
    fn test_demotable() {
        assert!(
            BlockSimError::InvalidTxRoot { got: Default::default(), expected: Default::default() }
                .is_demotable()
        )
    }
}
