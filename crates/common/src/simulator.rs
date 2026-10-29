use alloy_primitives::{Address, B256, U256};
use helix_telemetry::SimError;
use helix_types::{
    BidTrace, BlobsBundle, BlsSignatureBytes, ExecutionPayload, ExecutionRequests, ForkName,
    SignedBidSubmissionFulu,
};
use ssz::Encode as _;
use ssz_derive::{Decode, Encode};
use thiserror::Error;

use crate::{
    ValidatorPreferences, api::builder_api::InclusionListWithMetadata,
    decoder::SubmissionDecoderParams,
};

/// Wire format of `signed_bid_submission` in simulation requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum SubmissionFormat {
    /// Uncompressed SSZ `SignedBidSubmissionFulu` (current default).
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
const NO_STATE_FOR_BLOCK: &str = "no state found for block";
const MISSING_PARENT_STATE: &str = "parent state is not available";

/// `prefix` must be ASCII.
fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

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

    /// The relay's own cache could not resolve the submission's tx references. Distinct from
    /// `HydrationMiss`, which is the simulator's cache and drives the full-SSZ retry.
    #[error("relay hydration failed: unresolved transaction references")]
    RelayHydrationFailed,

    /// Not the builder's fault -- helix's own simulator client has no validation RPC method for
    /// this fork yet. See gattaca-com/helix#518.
    #[error("no validation RPC method for fork {0}")]
    UnsupportedFork(ForkName),

    /// The simulator refused the request body as too large. Says nothing about the block or
    /// about the node's health: every simulator with the same limit refuses it.
    #[error("simulator refused the request body as too large")]
    PayloadTooLarge,
}

impl From<&BlockSimError> for SimError {
    fn from(err: &BlockSimError) -> Self {
        match err {
            BlockSimError::BlockValidationFailed(_) => SimError::BlockValidationFailed,
            BlockSimError::InvalidTxRoot { .. } => SimError::InvalidTxRoot,
            BlockSimError::Timeout => SimError::Timeout,
            BlockSimError::RpcError => SimError::RpcError,
            BlockSimError::SendError => SimError::SendError,
            BlockSimError::NoSimulatorAvailable => SimError::NoSimulatorAvailable,
            BlockSimError::SimulationDropped => SimError::SimulationDropped,
            BlockSimError::HydrationMiss => SimError::HydrationMiss,
            BlockSimError::RelayHydrationFailed => SimError::RelayHydrationFailed,
            BlockSimError::UnsupportedFork(_) => SimError::UnsupportedFork,
            BlockSimError::PayloadTooLarge => SimError::PayloadTooLarge,
        }
    }
}

impl BlockSimError {
    pub fn is_temporary(&self) -> bool {
        match self {
            BlockSimError::BlockValidationFailed(reason) => {
                [
                    UNKNOWN_ANCESTOR,
                    PARENT_NOT_FOUND,
                    PARENT_BLOCK_NOT_FOUND,
                    BLOCK_REQ_REORG,
                    MISSING_PARENT_STATE,
                ]
                .iter()
                .any(|known| reason.eq_ignore_ascii_case(known)) ||
                    starts_with_ignore_ascii_case(reason, MISSING_TRIE_NODE) ||
                    starts_with_ignore_ascii_case(reason, NO_STATE_FOR_BLOCK)
            }
            BlockSimError::Timeout => true,
            BlockSimError::RpcError => true,
            BlockSimError::SimulationDropped => true,
            BlockSimError::NoSimulatorAvailable => true,
            BlockSimError::UnsupportedFork(_) => true,
            BlockSimError::RelayHydrationFailed => true,
            BlockSimError::PayloadTooLarge => true,
            _ => false,
        }
    }

    /// Whether this error says the simulator itself is unhealthy.
    ///
    /// A missing parent is temporary but is not a sim fault: at a slot boundary every node
    /// sees it until it imports the new head.
    pub fn is_sim_fault(&self) -> bool {
        match self {
            BlockSimError::BlockValidationFailed(reason) => {
                starts_with_ignore_ascii_case(reason, MISSING_TRIE_NODE)
            }
            BlockSimError::Timeout | BlockSimError::RpcError => true,
            _ => false,
        }
    }

    pub fn is_already_known(&self) -> bool {
        match self {
            BlockSimError::BlockValidationFailed(reason) => {
                reason.eq_ignore_ascii_case(BLOCK_ALREADY_KNOWN)
            }
            _ => false,
        }
    }

    pub fn is_too_old(&self) -> bool {
        match self {
            BlockSimError::BlockValidationFailed(reason) => {
                reason.eq_ignore_ascii_case(BLOCK_TOO_OLD)
            }
            _ => false,
        }
    }

    pub fn is_demotable(&self) -> bool {
        !self.is_already_known() && !self.is_temporary() && !self.is_too_old()
    }

    /// Whether a failed merged-block simulation is the merge builder's fault, and so should
    /// disable block merging, rather than a relay or simulator hiccup. `is_demotable` is
    /// calibrated for bid submissions and still counts relay-internal failures, which never
    /// say anything about the merged block.
    pub fn is_merge_builder_fault(&self) -> bool {
        self.is_demotable() &&
            !matches!(
                self,
                BlockSimError::SendError |
                    BlockSimError::SimulationDropped |
                    BlockSimError::HydrationMiss
            )
    }
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct SszValidationRequest {
    pub apply_blacklist: bool,
    pub registered_gas_limit: u64,
    pub parent_beacon_block_root: B256,
    pub inclusion_list: InclusionListWithMetadata,
    pub decoder_params: Option<SubmissionDecoderParams>,
    pub signed_bid_submission: Vec<u8>,
}

/// Merged-block counterpart of [`SszValidationRequest`], carrying the extra
/// `base_payment_tx_index` a merged-block-only validation endpoint uses to recognise the
/// base block's own payment tx directly, instead of scanning every tx in the block -- see
/// `BlockMergeResponse::base_payment_tx_index`.
#[derive(Debug, Clone, Encode, Decode)]
pub struct SszMergedValidationRequest {
    pub apply_blacklist: bool,
    pub registered_gas_limit: u64,
    pub parent_beacon_block_root: B256,
    pub inclusion_list: InclusionListWithMetadata,
    pub decoder_params: Option<SubmissionDecoderParams>,
    pub signed_bid_submission: Vec<u8>,
    pub base_payment_tx_index: u64,
}

/// Encode-only form of [`SszValidationRequest`] that borrows the submission and writes
/// it straight into the output. The derived container encoder stages every
/// variable-length field in a scratch `Vec` before appending it, which would copy a
/// multi-MB submission twice more. Encodes to the same bytes as the owned request.
pub struct SszValidationRequestRef<'a, S> {
    pub apply_blacklist: bool,
    pub registered_gas_limit: u64,
    pub parent_beacon_block_root: B256,
    pub inclusion_list: &'a InclusionListWithMetadata,
    pub decoder_params: Option<SubmissionDecoderParams>,
    pub signed_bid_submission: &'a S,
}

impl<S: ssz::Encode> SszValidationRequestRef<'_, S> {
    const FIXED_LEN: usize = 1 + 8 + 32 + 3 * ssz::BYTES_PER_LENGTH_OFFSET;

    /// `extra_fixed` is the fixed part of any fields declared after the submission. It sits
    /// between the submission's offset and the variable parts, so the submission stays the
    /// final bytes of the encoding.
    fn append_with(&self, extra_fixed: &[u8], buf: &mut Vec<u8>) {
        let mut offset = Self::FIXED_LEN + extra_fixed.len();
        self.apply_blacklist.ssz_append(buf);
        self.registered_gas_limit.ssz_append(buf);
        self.parent_beacon_block_root.ssz_append(buf);
        buf.extend_from_slice(&ssz::encode_length(offset));
        offset += self.inclusion_list.ssz_bytes_len();
        buf.extend_from_slice(&ssz::encode_length(offset));
        offset += self.decoder_params.ssz_bytes_len();
        buf.extend_from_slice(&ssz::encode_length(offset));
        buf.extend_from_slice(extra_fixed);
        self.inclusion_list.ssz_append(buf);
        self.decoder_params.ssz_append(buf);
        self.signed_bid_submission.ssz_append(buf);
    }

    fn bytes_len_with(&self, extra_fixed: usize) -> usize {
        Self::FIXED_LEN +
            extra_fixed +
            self.inclusion_list.ssz_bytes_len() +
            self.decoder_params.ssz_bytes_len() +
            self.signed_bid_submission.ssz_bytes_len()
    }
}

impl<S: ssz::Encode> ssz::Encode for SszValidationRequestRef<'_, S> {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn ssz_bytes_len(&self) -> usize {
        self.bytes_len_with(0)
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        self.append_with(&[], buf);
    }
}

/// Encode-only form of [`SszMergedValidationRequest`]; see [`SszValidationRequestRef`].
pub struct SszMergedValidationRequestRef<'a> {
    pub base: SszValidationRequestRef<'a, SignedBidSubmissionFulu>,
    pub base_payment_tx_index: u64,
}

impl ssz::Encode for SszMergedValidationRequestRef<'_> {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn ssz_bytes_len(&self) -> usize {
        self.base.bytes_len_with(size_of::<u64>())
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        self.base.append_with(&self.base_payment_tx_index.to_le_bytes(), buf);
    }
}

/// One transaction of a validated block.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct TxDetail {
    pub hash: B256,
    pub sender: Address,
    pub nonce: u64,
    /// `None` for a contract creation.
    pub to: Option<Address>,
    /// Coinbase balance rise across this transaction: the priority fee plus any
    /// value it moved to the coinbase. Zero when the coinbase spent more than it
    /// received, as in the builder's own payout.
    pub builder_payment: U256,
}

/// Body of a `200` from the ethrex validator's `/validate` and
/// `/validate_merged`: one entry per transaction, in block order.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct SszValidationResponse {
    pub txs: Vec<TxDetail>,
}

/// JSON-RPC params of a block validation. Borrows from the submission, so building
/// it clones nothing.
#[derive(Debug, serde::Serialize)]
pub struct JsonValidationRequest<'a> {
    #[serde(with = "serde_utils::quoted_u64")]
    pub registered_gas_limit: u64,
    pub message: &'a BidTrace,
    pub execution_payload: &'a ExecutionPayload,
    pub signature: &'a BlsSignatureBytes,
    pub proposer_preferences: ValidatorPreferences,
    pub blobs_bundle: &'a BlobsBundle,
    pub execution_requests: &'a ExecutionRequests,
    pub parent_beacon_block_root: B256,
    pub inclusion_list: &'a InclusionListWithMetadata,
    pub apply_blacklist: bool,
}

impl<'a> JsonValidationRequest<'a> {
    pub fn new(
        registered_gas_limit: u64,
        block: &'a SignedBidSubmissionFulu,
        proposer_preferences: ValidatorPreferences,
        parent_beacon_block_root: B256,
        inclusion_list: &'a InclusionListWithMetadata,
    ) -> Self {
        Self {
            registered_gas_limit,
            message: block.bid_trace(),
            execution_payload: block.execution_payload_ref(),
            signature: block.signature(),
            apply_blacklist: proposer_preferences.filtering.is_regional(),
            proposer_preferences,
            blobs_bundle: &block.blobs_bundle,
            execution_requests: block.execution_requests_ref(),
            parent_beacon_block_root,
            inclusion_list,
        }
    }
}

/// Merged-block counterpart of [`JsonValidationRequest`], carrying the extra
/// `base_payment_tx_index` -- see [`SszMergedValidationRequest`].
#[derive(Debug, serde::Serialize)]
pub struct MergedJsonValidationRequest<'a> {
    #[serde(flatten)]
    pub base: JsonValidationRequest<'a>,
    pub base_payment_tx_index: u64,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use helix_types::{
        GloasBidSubmission, GloasBidTrace, SignedBidSubmission, SignedBidSubmissionGloas, Slot,
    };
    use ssz::Encode;

    use super::*;
    use crate::api::builder_api::InclusionListTxWithMetadata;

    /// Simulators decode the borrowed requests as the derived owned ones, so the two must
    /// encode to the same bytes.
    #[test]
    fn borrowed_ssz_requests_encode_like_the_owned_ones() {
        // Deserializing the fixture's blob overflows the default test stack in debug builds.
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(borrowed_ssz_requests_encode_like_the_owned_ones_inner)
            .unwrap()
            .join()
            .unwrap();
    }

    fn borrowed_ssz_requests_encode_like_the_owned_ones_inner() {
        let fulu: SignedBidSubmissionFulu = serde_json::from_slice(include_bytes!(
            "../../types/src/testdata/signed-bid-submission-fulu-2.json"
        ))
        .unwrap();
        let gloas = SignedBidSubmissionGloas {
            message: GloasBidTrace { trace: fulu.message.clone(), parent_block_root: B256::ZERO },
            execution_payload: Arc::new(
                fulu.execution_payload
                    .to_lighthouse_gloas_payload(Slot::new(fulu.message.slot), &Default::default())
                    .unwrap(),
            ),
            blobs_bundle: fulu.blobs_bundle.clone(),
            execution_requests: Default::default(),
            signature: fulu.signature,
        };
        let gloas = GloasBidSubmission::from_ssz_bytes(gloas.as_ssz_bytes().into()).unwrap();
        let inclusion_list = InclusionListWithMetadata {
            txs: vec![InclusionListTxWithMetadata {
                hash: B256::repeat_byte(3),
                bytes: fulu.execution_payload.transactions[0].clone(),
            }],
        };
        let root = B256::repeat_byte(9);

        let cases = [
            (fulu.as_ssz_bytes(), SignedBidSubmission::Fulu(fulu.clone()), None),
            (
                gloas.as_bytes().to_vec(),
                SignedBidSubmission::Gloas(gloas),
                Some(SubmissionDecoderParams::plain(ForkName::Gloas)),
            ),
        ];
        for (submission_bytes, submission, decoder_params) in cases {
            let owned = SszValidationRequest {
                apply_blacklist: true,
                registered_gas_limit: 36_000_000,
                parent_beacon_block_root: root,
                inclusion_list: inclusion_list.clone(),
                decoder_params: decoder_params.clone(),
                signed_bid_submission: submission_bytes,
            }
            .as_ssz_bytes();
            let borrowed = SszValidationRequestRef {
                apply_blacklist: true,
                registered_gas_limit: 36_000_000,
                parent_beacon_block_root: root,
                inclusion_list: &inclusion_list,
                decoder_params,
                signed_bid_submission: &submission,
            };

            assert_eq!(borrowed.ssz_bytes_len(), owned.len());
            assert_eq!(borrowed.as_ssz_bytes(), owned);
        }

        let owned = SszMergedValidationRequest {
            apply_blacklist: false,
            registered_gas_limit: 36_000_000,
            parent_beacon_block_root: root,
            inclusion_list: inclusion_list.clone(),
            decoder_params: None,
            signed_bid_submission: fulu.as_ssz_bytes(),
            base_payment_tx_index: 7,
        }
        .as_ssz_bytes();
        let borrowed = SszMergedValidationRequestRef {
            base: SszValidationRequestRef {
                apply_blacklist: false,
                registered_gas_limit: 36_000_000,
                parent_beacon_block_root: root,
                inclusion_list: &inclusion_list,
                decoder_params: None,
                signed_bid_submission: &fulu,
            },
            base_payment_tx_index: 7,
        };

        assert_eq!(borrowed.ssz_bytes_len(), owned.len());
        assert_eq!(borrowed.as_ssz_bytes(), owned);
    }

    #[test]
    fn test_temporary() {
        let s = String::from("could not find parent block: parent block not found");
        let err = BlockSimError::BlockValidationFailed(s);

        assert!(err.is_temporary())
    }

    /// A parent the node has not imported yet is normal at a slot boundary. It says
    /// nothing about the node's health, so it must not count against the simulator.
    #[test]
    fn a_missing_parent_is_no_sim_fault() {
        for reason in [
            "unknown ancestor",
            "parent block not found",
            "could not find parent block: parent block not found",
            "block requires a reorg",
        ] {
            let err = BlockSimError::BlockValidationFailed(reason.into());

            assert!(err.is_temporary(), "{reason} must still retry elsewhere");
            assert!(!err.is_sim_fault(), "{reason} must not count against the sim");
        }
    }

    #[test]
    fn an_unreachable_sim_is_a_sim_fault() {
        assert!(BlockSimError::RpcError.is_sim_fault());
        assert!(BlockSimError::Timeout.is_sim_fault());
        assert!(
            BlockSimError::BlockValidationFailed("missing trie node abcd".into()).is_sim_fault()
        );
    }

    #[test]
    fn a_rejected_block_is_no_sim_fault() {
        let err = BlockSimError::BlockValidationFailed("insufficient balance".into());

        assert!(!err.is_sim_fault());
    }

    /// A 413 is our request meeting the simulator's body limit. The node answered in under a
    /// millisecond, so it must not arm the breaker, and the block was never judged, so it must
    /// not demote the builder.
    #[test]
    fn an_oversized_request_neither_demotes_nor_blames_the_simulator() {
        let err = BlockSimError::PayloadTooLarge;

        assert!(!err.is_sim_fault());
        assert!(!err.is_demotable());
    }

    #[test]
    fn a_dropped_simulation_never_demotes() {
        let err = BlockSimError::SimulationDropped;

        assert!(err.is_temporary());
        assert!(!err.is_demotable());
        assert!(!err.is_sim_fault());
    }

    #[test]
    fn test_demotable() {
        assert!(
            BlockSimError::InvalidTxRoot { got: Default::default(), expected: Default::default() }
                .is_demotable()
        )
    }

    /// The parent state can be missing while the simulator lags or reorgs, so it must not demote.
    #[test]
    fn missing_parent_state_never_demotes() {
        for reason in [
            "no state found for block 0xf9d64b1e6815113d8a761cac2d094802520a082b19221dcbeff2940b469adb7d",
            "parent state is not available",
        ] {
            let err = BlockSimError::BlockValidationFailed(reason.to_string());

            assert!(err.is_temporary(), "{err}");
            assert!(!err.is_demotable(), "{err}");
        }
    }

    /// A relay-side hydration failure is never the builder's fault, so it must not demote.
    #[test]
    fn relay_hydration_failure_never_demotes() {
        let err = BlockSimError::RelayHydrationFailed;

        assert!(err.is_temporary());
        assert!(!err.is_demotable());
    }
}
