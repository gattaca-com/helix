use alloy_primitives::{Address, B256, U256};
use helix_types::{
    Bloom, BlsPublicKey, BlsSignature, ChainSpec, ExecutionPayloadHeader,
    ExecutionPayloadHeaderElectra, ExecutionPayloadHeaderRef, ExecutionRequests, ExtraData,
    KzgCommitments, SigError, SignedMessage, SignedRoot, Slot, TestRandom,
};
use ssz_derive::{Decode, Encode};

use crate::bid_submission::{BidSubmission, BidTrace, BidValidationError};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Encode, Decode, TestRandom)]
pub struct HeaderSubmissionElectra {
    pub bid_trace: BidTrace,
    pub execution_payload_header: ExecutionPayloadHeaderElectra,
    pub execution_requests: ExecutionRequests,
    pub commitments: KzgCommitments,
}

pub type SignedHeaderSubmissionElectra = SignedMessage<HeaderSubmissionElectra>;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, Encode, Decode)]
#[ssz(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum SignedHeaderSubmission {
    Electra(SignedHeaderSubmissionElectra),
}

impl BidSubmission for SignedHeaderSubmission {
    fn bid_trace(&self) -> &BidTrace {
        match self {
            Self::Electra(signed_header_submission) => &signed_header_submission.message.bid_trace,
        }
    }

    fn signature(&self) -> &BlsSignature {
        match self {
            Self::Electra(signed_header_submission) => &signed_header_submission.signature,
        }
    }

    fn slot(&self) -> Slot {
        self.bid_trace().slot()
    }

    fn parent_hash(&self) -> &B256 {
        &self.bid_trace().parent_hash
    }

    fn block_hash(&self) -> &B256 {
        &self.bid_trace().block_hash
    }

    fn builder_public_key(&self) -> &BlsPublicKey {
        &self.bid_trace().builder_pubkey
    }

    fn proposer_public_key(&self) -> &BlsPublicKey {
        &self.bid_trace().proposer_pubkey
    }

    fn proposer_fee_recipient(&self) -> &Address {
        &self.bid_trace().proposer_fee_recipient
    }

    fn gas_limit(&self) -> u64 {
        self.bid_trace().gas_limit
    }

    fn gas_used(&self) -> u64 {
        self.bid_trace().gas_used
    }

    fn value(&self) -> U256 {
        self.bid_trace().value
    }

    fn fee_recipient(&self) -> Address {
        match self {
            Self::Electra(signed_header_submission) => {
                signed_header_submission.message.execution_payload_header.fee_recipient
            }
        }
    }

    fn state_root(&self) -> &B256 {
        match self {
            Self::Electra(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header.state_root
            }
        }
    }

    fn receipts_root(&self) -> &B256 {
        match self {
            Self::Electra(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header.receipts_root
            }
        }
    }

    fn logs_bloom(&self) -> &Bloom {
        match self {
            Self::Electra(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header.logs_bloom
            }
        }
    }

    fn prev_randao(&self) -> &B256 {
        match self {
            Self::Electra(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header.prev_randao
            }
        }
    }

    fn block_number(&self) -> u64 {
        match self {
            Self::Electra(signed_header_submission) => {
                signed_header_submission.message.execution_payload_header.block_number
            }
        }
    }

    fn timestamp(&self) -> u64 {
        match self {
            Self::Electra(signed_header_submission) => {
                signed_header_submission.message.execution_payload_header.timestamp
            }
        }
    }

    fn extra_data(&self) -> &ExtraData {
        match self {
            Self::Electra(signed_header_submission) => {
                &signed_header_submission.message.execution_payload_header.extra_data
            }
        }
    }

    fn base_fee_per_gas(&self) -> U256 {
        match self {
            Self::Electra(signed_header_submission) => {
                signed_header_submission.message.execution_payload_header.base_fee_per_gas
            }
        }
    }

    fn withdrawals_root(&self) -> B256 {
        match self {
            Self::Electra(signed_header_submission) => {
                signed_header_submission.message.execution_payload_header.withdrawals_root
            }
        }
    }

    fn transactions_root(&self) -> B256 {
        match self {
            Self::Electra(signed_header_submission) => {
                signed_header_submission.message.execution_payload_header.transactions_root
            }
        }
    }

    fn is_full_payload(&self) -> bool {
        false
    }

    fn validate(&self) -> Result<(), BidValidationError> {
        let bid_trace = self.bid_trace();

        let execution_payload_header: ExecutionPayloadHeaderRef = match self {
            SignedHeaderSubmission::Electra(bid) => (&bid.message.execution_payload_header).into(),
        };

        if bid_trace.parent_hash != execution_payload_header.parent_hash().0 {
            return Err(BidValidationError::ParentHashMismatch {
                message: bid_trace.parent_hash,
                payload: execution_payload_header.parent_hash().0,
            });
        }

        if bid_trace.block_hash != execution_payload_header.block_hash().0 {
            return Err(BidValidationError::BlockHashMismatch {
                message: bid_trace.block_hash,
                payload: execution_payload_header.block_hash().0,
            });
        }

        if bid_trace.gas_limit != execution_payload_header.gas_limit() {
            return Err(BidValidationError::GasLimitMismatch {
                message: bid_trace.gas_limit,
                payload: execution_payload_header.gas_limit(),
            });
        }

        if bid_trace.gas_used != execution_payload_header.gas_used() {
            return Err(BidValidationError::GasUsedMismatch {
                message: bid_trace.gas_used,
                payload: execution_payload_header.gas_used(),
            });
        }

        if bid_trace.value == U256::ZERO {
            return Err(BidValidationError::ZeroValueBlock);
        }

        Ok(())
    }

    fn fork_name(&self) -> helix_types::ForkName {
        match self {
            Self::Electra(_) => helix_types::ForkName::Electra,
        }
    }
}

impl SignedHeaderSubmission {
    pub fn verify_signature(&self, spec: &ChainSpec) -> Result<(), SigError> {
        let domain = spec.get_builder_domain();
        let valid = match self {
            SignedHeaderSubmission::Electra(bid) => {
                let message = bid.message.bid_trace.signing_root(domain);
                bid.signature.verify(&bid.message.bid_trace.builder_pubkey, message)
            }
        };

        if !valid {
            return Err(SigError::InvalidBlsSignature);
        }

        Ok(())
    }

    pub fn execution_payload_header(&self) -> ExecutionPayloadHeader {
        match self {
            Self::Electra(signed_header_submission) => {
                signed_header_submission.message.execution_payload_header.clone().into()
            }
        }
    }

    pub fn commitments(&self) -> &KzgCommitments {
        match self {
            Self::Electra(signed_header_submission) => {
                &signed_header_submission.message.commitments
            }
        }
    }

    pub fn bid_trace(&self) -> &BidTrace {
        match self {
            Self::Electra(signed_header_submission) => &signed_header_submission.message.bid_trace,
        }
    }

    pub fn execution_requests(&self) -> Option<&ExecutionRequests> {
        match self {
            Self::Electra(signed_header_submission) => {
                Some(&signed_header_submission.message.execution_requests)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ssz::Decode;

    use super::*;

    #[test]
    fn test_deserialise_signed_header_submission() {
        let bytes: Vec<u8> = vec![
            100, 0, 0, 0, 93, 31, 4, 4, 6, 1, 1, 24, 145, 34, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0,
            0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 244, 0, 0, 0, 63, 3, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 134, 1, 2, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 44, 66, 11, 77,
            22, 88, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 1, 2, 4, 5, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 1, 0, 0, 0, 0, 0, 0, 0, 167, 1, 0, 0, 0, 0, 0, 0, 37, 91, 0, 0, 0, 0, 0, 0, 29, 2,
            0, 0, 0, 0, 0, 0, 72, 2, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 22, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 35, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 86, 1, 0, 0, 0, 0, 0,
            0, 18, 9, 0, 0, 0, 0, 0, 0, 4, 5, 6, 1, 2, 4, 56, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ];

        let result = SignedHeaderSubmission::from_ssz_bytes(&bytes);
        assert!(result.is_ok());
        println!("{:?}", result.unwrap());
    }
}
