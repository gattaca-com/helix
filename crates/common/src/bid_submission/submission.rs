use alloy_primitives::{Address, B256, U256};
use helix_types::{
    BidTrace, Bloom, BlsPublicKey, BlsSignature, ExecutionPayloadRef, ExtraData,
    SignedBidSubmission, Slot,
};
use tree_hash::TreeHash;

use super::BidValidationError;
use crate::bid_submission::BidSubmission;

impl BidSubmission for SignedBidSubmission {
    fn bid_trace(&self) -> &BidTrace {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => &signed_bid_submission.message,
        }
    }

    fn signature(&self) -> &BlsSignature {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => &signed_bid_submission.signature,
        }
    }

    fn slot(&self) -> Slot {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.message.slot()
            }
        }
    }

    fn parent_hash(&self) -> &B256 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.parent_hash
            }
        }
    }

    fn block_hash(&self) -> &B256 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.block_hash
            }
        }
    }

    fn builder_public_key(&self) -> &BlsPublicKey {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.builder_pubkey
            }
        }
    }

    fn proposer_public_key(&self) -> &BlsPublicKey {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_pubkey
            }
        }
    }

    fn proposer_fee_recipient(&self) -> &Address {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.message.proposer_fee_recipient
            }
        }
    }

    fn gas_limit(&self) -> u64 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.message.gas_limit
            }
        }
    }

    fn gas_used(&self) -> u64 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.message.gas_used
            }
        }
    }

    fn value(&self) -> U256 {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.message.value
            }
        }
    }

    fn fee_recipient(&self) -> Address {
        match self {
            SignedBidSubmission::Electra(bid) => bid.execution_payload.fee_recipient,
        }
    }

    fn state_root(&self) -> &B256 {
        match self {
            SignedBidSubmission::Electra(bid) => &bid.execution_payload.state_root,
        }
    }

    fn receipts_root(&self) -> &B256 {
        match self {
            SignedBidSubmission::Electra(bid) => &bid.execution_payload.receipts_root,
        }
    }

    fn logs_bloom(&self) -> &Bloom {
        match self {
            SignedBidSubmission::Electra(bid) => &bid.execution_payload.logs_bloom,
        }
    }

    fn prev_randao(&self) -> &B256 {
        match self {
            SignedBidSubmission::Electra(bid) => &bid.execution_payload.prev_randao,
        }
    }

    fn block_number(&self) -> u64 {
        match self {
            SignedBidSubmission::Electra(bid) => bid.execution_payload.block_number,
        }
    }

    fn timestamp(&self) -> u64 {
        match self {
            SignedBidSubmission::Electra(bid) => bid.execution_payload.timestamp,
        }
    }

    fn extra_data(&self) -> &ExtraData {
        match self {
            SignedBidSubmission::Electra(bid) => &bid.execution_payload.extra_data,
        }
    }

    fn base_fee_per_gas(&self) -> U256 {
        match self {
            SignedBidSubmission::Electra(bid) => bid.execution_payload.base_fee_per_gas,
        }
    }

    fn withdrawals_root(&self) -> B256 {
        match self {
            SignedBidSubmission::Electra(bid) => bid.execution_payload.withdrawals.tree_hash_root(),
        }
    }

    fn transactions_root(&self) -> B256 {
        match self {
            SignedBidSubmission::Electra(bid) => {
                bid.execution_payload.transactions.tree_hash_root()
            }
        }
    }

    fn is_full_payload(&self) -> bool {
        true
    }

    fn validate(&self) -> Result<(), super::BidValidationError> {
        let bid_trace = self.bid_trace();
        let execution_payload: ExecutionPayloadRef = match self {
            SignedBidSubmission::Electra(bid) => (&bid.execution_payload).into(),
        };

        if bid_trace.parent_hash != execution_payload.parent_hash().0 {
            return Err(BidValidationError::ParentHashMismatch {
                message: bid_trace.parent_hash,
                payload: execution_payload.parent_hash().0,
            });
        }

        if bid_trace.block_hash != execution_payload.block_hash().0 {
            return Err(BidValidationError::BlockHashMismatch {
                message: bid_trace.block_hash,
                payload: execution_payload.block_hash().0,
            });
        }

        if bid_trace.gas_limit != execution_payload.gas_limit() {
            return Err(BidValidationError::GasLimitMismatch {
                message: bid_trace.gas_limit,
                payload: execution_payload.gas_limit(),
            });
        }

        if bid_trace.gas_used != execution_payload.gas_used() {
            return Err(BidValidationError::GasUsedMismatch {
                message: bid_trace.gas_used,
                payload: execution_payload.gas_used(),
            });
        }

        if bid_trace.value == U256::ZERO {
            return Err(BidValidationError::ZeroValueBlock);
        }

        Ok(())
    }

    fn fork_name(&self) -> helix_types::ForkName {
        match self {
            SignedBidSubmission::Electra(_) => helix_types::ForkName::Electra,
        }
    }
}
