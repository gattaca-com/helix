use std::{cmp::Ordering, sync::Arc};

use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::beacon::relay::SignedBidSubmissionV5;
use lh_types::{ForkName, SignedRoot, Slot, test_utils::TestRandom};
use serde::{Deserialize, Serialize};
use ssz::{Decode, DecodeError};
use ssz_derive::{Decode, Encode};
use tree_hash::TreeHash;
use tree_hash_derive::TreeHash;

use crate::{
    BlobsBundle, BlobsError, Bloom, BlsPublicKey, BlsPublicKeyBytes, BlsSignature,
    BlsSignatureBytes, DehydratedBidSubmission, ExecutionPayload, ExtraData, PayloadAndBlobs,
    SszError, bid_adjustment_data::BidAdjustmentData, error::SigError, fields::ExecutionRequests,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, TreeHash)]
#[serde(deny_unknown_fields)]
pub struct BidTrace {
    /// The slot associated with the block.
    #[serde(with = "serde_utils::quoted_u64")]
    pub slot: u64,
    /// The parent hash of the block.
    pub parent_hash: B256,
    /// The hash of the block.
    pub block_hash: B256,
    /// The public key of the builder.
    pub builder_pubkey: BlsPublicKeyBytes,
    /// The public key of the proposer.
    pub proposer_pubkey: BlsPublicKeyBytes,
    /// The recipient of the proposer's fee.
    pub proposer_fee_recipient: Address,
    /// The gas limit associated with the block.
    #[serde(with = "serde_utils::quoted_u64")]
    pub gas_limit: u64,
    /// The gas used within the block.
    #[serde(with = "serde_utils::quoted_u64")]
    pub gas_used: u64,
    /// The value associated with the block.
    #[serde(with = "serde_utils::quoted_u256")]
    pub value: U256,
}

impl TestRandom for BidTrace {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            slot: u64::random_for_test(rng),
            parent_hash: B256::random_for_test(rng),
            block_hash: B256::random_for_test(rng),
            builder_pubkey: BlsPublicKeyBytes::random(),
            proposer_pubkey: BlsPublicKeyBytes::random(),
            proposer_fee_recipient: Address::random_for_test(rng),
            gas_limit: u64::random_for_test(rng),
            gas_used: u64::random_for_test(rng),
            value: U256::random_for_test(rng),
        }
    }
}

impl SignedRoot for BidTrace {}

impl BidTrace {
    pub fn slot(&self) -> Slot {
        Slot::from(self.slot)
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Submission {
    // received after sigverify
    Full(SignedBidSubmission),
    // need to validate do the validate_payload_ssz_lengths
    Dehydrated(DehydratedBidSubmission),
}

impl Submission {
    pub fn bid_slot(&self) -> u64 {
        match self {
            Submission::Full(s) => s.slot().as_u64(),
            Submission::Dehydrated(s) => s.slot(),
        }
    }

    pub fn builder_pubkey(&self) -> &BlsPublicKeyBytes {
        match self {
            Submission::Full(s) => &s.message().builder_pubkey,
            Submission::Dehydrated(s) => s.builder_pubkey(),
        }
    }

    pub fn block_hash(&self) -> &B256 {
        match self {
            Submission::Full(s) => &s.message().block_hash,
            Submission::Dehydrated(s) => s.block_hash(),
        }
    }

    pub fn withdrawal_root(&self) -> B256 {
        match self {
            Submission::Full(s) => s.withdrawals_root(),
            Submission::Dehydrated(s) => s.withdrawal_root(),
        }
    }

    pub fn parent_hash(&self) -> &B256 {
        match self {
            Submission::Full(s) => s.parent_hash(),
            Submission::Dehydrated(s) => s.parent_hash(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Encode)]
#[serde(deny_unknown_fields)]
pub struct SignedBidSubmission {
    pub message: BidTrace,
    pub execution_payload: Arc<ExecutionPayload>,
    pub blobs_bundle: Arc<BlobsBundle>,
    pub execution_requests: Arc<ExecutionRequests>,
    pub signature: BlsSignatureBytes,
}

impl TestRandom for SignedBidSubmission {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            message: BidTrace::random_for_test(rng),
            execution_payload: ExecutionPayload::random_for_test(rng).into(),
            blobs_bundle: BlobsBundle::random_for_test(rng).into(),
            execution_requests: ExecutionRequests::random_for_test(rng).into(),
            signature: BlsSignatureBytes::random(),
        }
    }
}

impl From<SignedBidSubmissionV5> for SignedBidSubmission {
    fn from(v: SignedBidSubmissionV5) -> SignedBidSubmission {
        use crate::fields::{Transaction, Withdrawal};
        let m = v.message;
        let v2 = v.execution_payload.payload_inner;
        let v1 = v2.payload_inner;
        SignedBidSubmission {
            message: BidTrace {
                slot: m.slot,
                parent_hash: m.parent_hash,
                block_hash: m.block_hash,
                builder_pubkey: m.builder_pubkey,
                proposer_pubkey: m.proposer_pubkey,
                proposer_fee_recipient: m.proposer_fee_recipient,
                gas_limit: m.gas_limit,
                gas_used: m.gas_used,
                value: m.value,
            },
            execution_payload: Arc::new(ExecutionPayload {
                parent_hash: v1.parent_hash,
                fee_recipient: v1.fee_recipient,
                state_root: v1.state_root,
                receipts_root: v1.receipts_root,
                logs_bloom: *v1.logs_bloom,
                prev_randao: v1.prev_randao,
                block_number: v1.block_number,
                gas_limit: v1.gas_limit,
                gas_used: v1.gas_used,
                timestamp: v1.timestamp,
                extra_data: ExtraData(v1.extra_data),
                base_fee_per_gas: v1.base_fee_per_gas,
                block_hash: v1.block_hash,
                transactions: lh_types::VariableList::new(
                    v1.transactions.into_iter().map(Transaction).collect(),
                )
                .expect("transactions exceed spec limit"),
                withdrawals: lh_types::VariableList::new(
                    v2.withdrawals
                        .into_iter()
                        .map(|w| Withdrawal {
                            index: w.index,
                            validator_index: w.validator_index,
                            address: w.address,
                            amount: w.amount,
                        })
                        .collect(),
                )
                .expect("withdrawals exceed spec limit"),
                blob_gas_used: v.execution_payload.blob_gas_used,
                excess_blob_gas: v.execution_payload.excess_blob_gas,
            }),
            blobs_bundle: Arc::new(BlobsBundle {
                commitments: lh_types::VariableList::new(v.blobs_bundle.commitments)
                    .expect("commitments exceed spec limit"),
                proofs: v.blobs_bundle.proofs,
                blobs: v.blobs_bundle.blobs.into_iter().map(Arc::new).collect(),
            }),
            execution_requests: Arc::new(ExecutionRequests {
                deposits: lh_types::VariableList::new(
                    v.execution_requests
                        .deposits
                        .into_iter()
                        .map(|d| lh_types::DepositRequest {
                            pubkey: lh_types::PublicKeyBytes::deserialize(&d.pubkey[..])
                                .expect("len=48"),
                            withdrawal_credentials: d.withdrawal_credentials,
                            amount: d.amount,
                            signature: lh_types::SignatureBytes::deserialize(&d.signature[..])
                                .expect("len=96"),
                            index: d.index,
                        })
                        .collect(),
                )
                .expect("deposits exceed spec limit"),
                withdrawals: lh_types::VariableList::new(
                    v.execution_requests
                        .withdrawals
                        .into_iter()
                        .map(|w| lh_types::WithdrawalRequest {
                            source_address: w.source_address,
                            validator_pubkey: lh_types::PublicKeyBytes::deserialize(
                                &w.validator_pubkey[..],
                            )
                            .expect("len=48"),
                            amount: w.amount,
                        })
                        .collect(),
                )
                .expect("withdrawal requests exceed spec limit"),
                consolidations: lh_types::VariableList::new(
                    v.execution_requests
                        .consolidations
                        .into_iter()
                        .map(|c| lh_types::ConsolidationRequest {
                            source_address: c.source_address,
                            source_pubkey: lh_types::PublicKeyBytes::deserialize(
                                &c.source_pubkey[..],
                            )
                            .expect("len=48"),
                            target_pubkey: lh_types::PublicKeyBytes::deserialize(
                                &c.target_pubkey[..],
                            )
                            .expect("len=48"),
                        })
                        .collect(),
                )
                .expect("consolidations exceed spec limit"),
            }),
            signature: v.signature,
        }
    }
}

impl From<SignedBidSubmission> for SignedBidSubmissionV5 {
    fn from(s: SignedBidSubmission) -> SignedBidSubmissionV5 {
        use alloy_eips::{
            eip4895::Withdrawal as AlloyWithdrawal, eip6110::DepositRequest as AlloyDepositRequest,
            eip7002::WithdrawalRequest as AlloyWithdrawalRequest,
            eip7251::ConsolidationRequest as AlloyConsolidationRequest,
        };
        // alloy re-exports this as the same type but with a different struct name
        use alloy_rpc_types::beacon::relay::BidTrace as AlloyBidTrace;
        use alloy_rpc_types::{
            beacon::requests::ExecutionRequestsV4,
            engine::{BlobsBundleV2, ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3},
        };
        let ep = &*s.execution_payload;
        let bb = &*s.blobs_bundle;
        let er = &*s.execution_requests;
        let m = s.message;
        SignedBidSubmissionV5 {
            message: AlloyBidTrace {
                slot: m.slot,
                parent_hash: m.parent_hash,
                block_hash: m.block_hash,
                builder_pubkey: m.builder_pubkey,
                proposer_pubkey: m.proposer_pubkey,
                proposer_fee_recipient: m.proposer_fee_recipient,
                gas_limit: m.gas_limit,
                gas_used: m.gas_used,
                value: m.value,
            },
            execution_payload: ExecutionPayloadV3 {
                payload_inner: ExecutionPayloadV2 {
                    payload_inner: ExecutionPayloadV1 {
                        parent_hash: ep.parent_hash,
                        fee_recipient: ep.fee_recipient,
                        state_root: ep.state_root,
                        receipts_root: ep.receipts_root,
                        logs_bloom: alloy_primitives::Bloom(ep.logs_bloom),
                        prev_randao: ep.prev_randao,
                        block_number: ep.block_number,
                        gas_limit: ep.gas_limit,
                        gas_used: ep.gas_used,
                        timestamp: ep.timestamp,
                        extra_data: ep.extra_data.0.clone(),
                        base_fee_per_gas: ep.base_fee_per_gas,
                        block_hash: ep.block_hash,
                        transactions: ep.transactions.iter().map(|t| t.0.clone()).collect(),
                    },
                    withdrawals: ep
                        .withdrawals
                        .iter()
                        .map(|w| AlloyWithdrawal {
                            index: w.index,
                            validator_index: w.validator_index,
                            address: w.address,
                            amount: w.amount,
                        })
                        .collect(),
                },
                blob_gas_used: ep.blob_gas_used,
                excess_blob_gas: ep.excess_blob_gas,
            },
            blobs_bundle: BlobsBundleV2 {
                commitments: bb.commitments.iter().cloned().collect(),
                proofs: bb.proofs.clone(),
                blobs: bb.blobs.iter().map(|b| (**b).clone()).collect(),
            },
            execution_requests: ExecutionRequestsV4 {
                deposits: er
                    .deposits
                    .iter()
                    .map(|d| AlloyDepositRequest {
                        pubkey: d.pubkey.serialize().into(),
                        withdrawal_credentials: d.withdrawal_credentials,
                        amount: d.amount,
                        signature: d.signature.serialize().into(),
                        index: d.index,
                    })
                    .collect(),
                withdrawals: er
                    .withdrawals
                    .iter()
                    .map(|w| AlloyWithdrawalRequest {
                        source_address: w.source_address,
                        validator_pubkey: w.validator_pubkey.serialize().into(),
                        amount: w.amount,
                    })
                    .collect(),
                consolidations: er
                    .consolidations
                    .iter()
                    .map(|c| AlloyConsolidationRequest {
                        source_address: c.source_address,
                        source_pubkey: c.source_pubkey.serialize().into(),
                        target_pubkey: c.target_pubkey.serialize().into(),
                    })
                    .collect(),
            },
            signature: s.signature,
        }
    }
}

impl<'de> Deserialize<'de> for SignedBidSubmission {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            message: BidTrace,
            execution_payload: Arc<ExecutionPayload>,
            blobs_bundle: BlobsBundle,
            execution_requests: Arc<ExecutionRequests>,
            signature: BlsSignatureBytes,
        }

        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            message: raw.message,
            execution_payload: raw.execution_payload,
            blobs_bundle: Arc::new(raw.blobs_bundle),
            execution_requests: raw.execution_requests,
            signature: raw.signature,
        })
    }
}

impl Decode for SignedBidSubmission {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        use ssz::SszDecoderBuilder;

        let mut builder = SszDecoderBuilder::new(bytes);
        builder.register_type::<BidTrace>()?;
        builder.register_anonymous_variable_length_item()?; // execution_payload
        builder.register_type::<BlobsBundle>()?; // blobs_bundle
        builder.register_anonymous_variable_length_item()?; // execution_requests
        builder.register_type::<BlsSignatureBytes>()?; // signature

        let mut decoder = builder.build()?;

        let message = decoder.decode_next::<BidTrace>()?;
        let execution_payload = decoder.decode_next::<Arc<ExecutionPayload>>()?;
        let blobs_bundle_v2 = decoder.decode_next::<BlobsBundle>()?;
        let execution_requests = decoder.decode_next::<Arc<ExecutionRequests>>()?;
        let signature = decoder.decode_next::<BlsSignatureBytes>()?;

        Ok(Self {
            message,
            execution_payload,
            blobs_bundle: Arc::new(blobs_bundle_v2),
            execution_requests,
            signature,
        })
    }
}

impl SignedBidSubmission {
    pub fn validate_payload_ssz_lengths(
        &self,
        max_blobs_per_block: usize,
    ) -> Result<(), BlockValidationError> {
        self.execution_payload.validate_ssz_lengths()?;
        self.blobs_bundle.validate_ssz_lengths(max_blobs_per_block)?;

        Ok(())
    }

    pub fn verify_signature(&self, builder_domain: B256) -> Result<(), SigError> {
        let uncompressed_builder_pubkey =
            BlsPublicKey::deserialize(self.message.builder_pubkey.as_slice())
                .map_err(|_| SigError::InvalidBlsPubkeyBytes)?;
        let uncompressed_signature = BlsSignature::deserialize(self.signature.as_slice())
            .map_err(|_| SigError::InvalidBlsSignatureBytes)?;

        let message = self.message.signing_root(builder_domain);
        let valid = uncompressed_signature.verify(&uncompressed_builder_pubkey, message);

        if !valid {
            return Err(SigError::InvalidBlsSignature);
        }

        Ok(())
    }

    pub fn num_txs(&self) -> usize {
        self.execution_payload.transactions.len()
    }

    pub fn blobs_bundle(&self) -> Arc<BlobsBundle> {
        self.blobs_bundle.clone()
    }

    pub fn message(&self) -> &BidTrace {
        &self.message
    }

    pub fn message_mut(&mut self) -> &mut BidTrace {
        &mut self.message
    }

    pub fn execution_payload_ref(&self) -> &ExecutionPayload {
        &self.execution_payload
    }

    pub fn execution_payload_make_mut(&mut self) -> &mut ExecutionPayload {
        Arc::make_mut(&mut self.execution_payload)
    }

    pub fn execution_payload_mut(&mut self) -> &mut ExecutionPayload {
        self.execution_payload_make_mut()
    }

    pub fn payload_and_blobs(&self) -> PayloadAndBlobs {
        PayloadAndBlobs {
            execution_payload: self.execution_payload.clone(),
            blobs_bundle: self.blobs_bundle.clone(),
        }
    }

    pub fn execution_requests_ref(&self) -> &Arc<ExecutionRequests> {
        &self.execution_requests
    }
}

impl SignedBidSubmission {
    pub fn bid_trace(&self) -> &BidTrace {
        &self.message
    }

    pub fn signature(&self) -> &BlsSignatureBytes {
        &self.signature
    }

    pub fn set_signature(&mut self, signature: BlsSignatureBytes) {
        self.signature = signature;
    }

    pub fn slot(&self) -> Slot {
        self.message.slot()
    }

    pub fn parent_hash(&self) -> &B256 {
        &self.message.parent_hash
    }

    pub fn block_hash(&self) -> &B256 {
        &self.message.block_hash
    }

    pub fn builder_public_key(&self) -> &BlsPublicKeyBytes {
        &self.message.builder_pubkey
    }

    pub fn set_builder_public_key(&mut self, builder_pubkey: BlsPublicKeyBytes) {
        self.message.builder_pubkey = builder_pubkey;
    }

    pub fn proposer_public_key(&self) -> &BlsPublicKeyBytes {
        &self.message.proposer_pubkey
    }

    pub fn proposer_fee_recipient(&self) -> &Address {
        &self.message.proposer_fee_recipient
    }

    pub fn gas_limit(&self) -> u64 {
        self.message.gas_limit
    }

    pub fn gas_used(&self) -> u64 {
        self.message.gas_used
    }

    pub fn value(&self) -> &U256 {
        &self.message.value
    }

    pub fn fee_recipient(&self) -> Address {
        self.execution_payload.fee_recipient
    }

    pub fn state_root(&self) -> &B256 {
        &self.execution_payload.state_root
    }

    pub fn receipts_root(&self) -> &B256 {
        &self.execution_payload.receipts_root
    }

    pub fn logs_bloom(&self) -> &Bloom {
        &self.execution_payload.logs_bloom
    }

    pub fn prev_randao(&self) -> &B256 {
        &self.execution_payload.prev_randao
    }

    pub fn block_number(&self) -> u64 {
        self.execution_payload.block_number
    }

    pub fn timestamp(&self) -> u64 {
        self.execution_payload.timestamp
    }

    pub fn extra_data(&self) -> &ExtraData {
        &self.execution_payload.extra_data
    }

    pub fn base_fee_per_gas(&self) -> U256 {
        self.execution_payload.base_fee_per_gas
    }

    pub fn withdrawals_root(&self) -> B256 {
        self.execution_payload.withdrawals.tree_hash_root()
    }

    pub fn transactions_root(&self) -> B256 {
        self.execution_payload.transaction_root()
    }

    pub fn validate(&self) -> Result<(), super::BlockValidationError> {
        let bid_trace = self.bid_trace();
        let execution_payload = self.execution_payload_ref();

        if bid_trace.parent_hash != execution_payload.parent_hash {
            return Err(BlockValidationError::ParentHashMismatch {
                message: bid_trace.parent_hash,
                payload: execution_payload.parent_hash,
            });
        }

        if bid_trace.block_hash != execution_payload.block_hash {
            return Err(BlockValidationError::BlockHashMismatch {
                message: bid_trace.block_hash,
                payload: execution_payload.block_hash,
            });
        }

        if bid_trace.gas_limit != execution_payload.gas_limit {
            return Err(BlockValidationError::GasLimitMismatch {
                message: bid_trace.gas_limit,
                payload: execution_payload.gas_limit,
            });
        }

        if bid_trace.gas_used != execution_payload.gas_used {
            return Err(BlockValidationError::GasUsedMismatch {
                message: bid_trace.gas_used,
                payload: execution_payload.gas_used,
            });
        }

        if bid_trace.value == U256::ZERO {
            return Err(BlockValidationError::ZeroValueBlock);
        }

        Ok(())
    }

    pub fn fork_name(&self) -> ForkName {
        ForkName::Fulu
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct SignedBidSubmissionWithAdjustments {
    pub message: BidTrace,
    pub execution_payload: Arc<ExecutionPayload>,
    pub blobs_bundle: Arc<BlobsBundle>,
    pub execution_requests: Arc<ExecutionRequests>,
    pub signature: BlsSignatureBytes,
    pub bid_adjustment_data: BidAdjustmentData,
}

impl SignedBidSubmissionWithAdjustments {
    pub fn split(self) -> (SignedBidSubmission, BidAdjustmentData) {
        (
            SignedBidSubmission {
                message: self.message,
                execution_payload: self.execution_payload,
                blobs_bundle: self.blobs_bundle,
                execution_requests: self.execution_requests,
                signature: self.signature,
            },
            self.bid_adjustment_data,
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SubmissionVersion {
    on_receive_ns: u64,
    sequence: Option<u32>,
}

impl SubmissionVersion {
    pub fn new(on_receive_ns: u64, sequence: Option<u32>) -> Self {
        Self { on_receive_ns, sequence }
    }

    pub fn on_receive_ns(&self) -> u64 {
        self.on_receive_ns
    }
}

impl std::fmt::Debug for SubmissionVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(seq) = self.sequence {
            write!(f, "(recv_ns: {}, seq: {})", self.on_receive_ns, seq)
        } else {
            write!(f, "(recv_ns: {})", self.on_receive_ns)
        }
    }
}

impl Ord for SubmissionVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.sequence, other.sequence) {
            (Some(a), Some(b)) => a.cmp(&b),
            _ => self.on_receive_ns.cmp(&other.on_receive_ns),
        }
    }
}

impl PartialOrd for SubmissionVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum BlockValidationError {
    #[error("submission for wrong slot. expected: {expected}, got: {got}")]
    SubmissionForWrongSlot { expected: Slot, got: Slot },

    #[error("fee recipient mismatch. got: {got:?}, expected: {expected:?}")]
    FeeRecipientMismatch { got: Address, expected: Address },

    #[error("proposer public key mismatch. got: {got:?}, expected: {expected:?}")]
    ProposerPublicKeyMismatch { got: BlsPublicKeyBytes, expected: BlsPublicKeyBytes },

    #[error("slot mismatch. got: {got}, expected: {expected}")]
    SlotMismatch { got: u64, expected: u64 },

    #[error("block hash mismatch: message: {message:?}, payload: {payload:?}")]
    BlockHashMismatch { message: B256, payload: B256 },

    #[error("parent hash mismatch. message: {message:?}, payload: {payload:?}")]
    ParentHashMismatch { message: B256, payload: B256 },

    #[error("gas limit mismatch. message: {message:?}, payload: {payload:?}")]
    GasLimitMismatch { message: u64, payload: u64 },

    #[error("gas used mismatch. message: {message:?}, payload: {payload:?}")]
    GasUsedMismatch { message: u64, payload: u64 },

    #[error("withdrawls root mismatch. got: {got:?}, expected: {expected:?}")]
    WithdrawalsRootMismatch { got: B256, expected: B256 },

    #[error("transactions root mismatch. got: {got:?}, expected: {expected:?}")]
    TransactionsRootMismatch { got: B256, expected: B256 },

    #[error("incorrect prev_randao - got: {got:?}, expected: {expected:?}")]
    PrevRandaoMismatch { got: B256, expected: B256 },

    #[error("unknown parent hash: submission: {submission}, have: {have:?}")]
    UknnownParentHash { submission: B256, have: Vec<B256> },

    #[error("not {fork_name:?} payload")]
    InvalidPayloadType { fork_name: ForkName },

    #[error("incorrect timestamp. got: {got}, expected: {expected}")]
    IncorrectTimestamp { got: u64, expected: u64 },

    #[error("already processing newer payload for this builder")]
    AlreadyProcessingNewerPayload,

    #[error("out of sequence submission: seen: {seen:?}, this request: {this:?}")]
    OutOfSequence { seen: SubmissionVersion, this: SubmissionVersion },

    #[error("zero value block")]
    ZeroValueBlock,

    #[error("builder not in proposer's trusted list: {proposer_trusted_builders:?}")]
    BuilderNotInProposersTrustedList { proposer_trusted_builders: Vec<String> },

    #[error("ssz_error: {0:?}")]
    SszError(SszError),

    #[error(transparent)]
    BlobsError(BlobsError),
}

#[cfg(test)]
mod tests {
    use ssz::Encode;

    use super::*;
    use crate::test_utils::{test_encode_decode_json, test_encode_decode_ssz};

    #[test]
    // from the relay API spec, adding the blob and the proposer_pubkey field
    fn fulu_bid_submission() {
        let data_json = include_str!("testdata/signed-bid-submission-fulu.json");
        let s = test_encode_decode_json::<SignedBidSubmission>(data_json);
        assert_eq!(s.fork_name(), ForkName::Fulu);
    }

    #[test]
    fn fulu_bid_submission_ssz() {
        let data_ssz = SignedBidSubmission::random_for_test(&mut rand::rng()).as_ssz_bytes();
        let s = test_encode_decode_ssz::<SignedBidSubmission>(&data_ssz);
        assert_eq!(data_ssz, s.as_ssz_bytes().as_slice());
        assert_eq!(s.fork_name(), ForkName::Fulu);
    }
}
