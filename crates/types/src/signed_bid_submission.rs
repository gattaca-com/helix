use alloy_primitives::{Address, B256, U256};

use lh_types::{ExecutionPayloadDeneb, ExecutionPayloadElectra, MainnetEthSpec, SignedRoot, Slot};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

use crate::{
    error::SigError, BlobsBundle, BlsPublicKey, BlsSignature, ChainSpec, ExecutionPayload,
    ExecutionRequests, PayloadAndBlobs,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode, TreeHash)]
pub struct BidTrace {
    /// The slot associated with the block.
    #[serde(with = "serde_utils::quoted_u64")]
    pub slot: u64,
    /// The parent hash of the block.
    pub parent_hash: B256,
    /// The hash of the block.
    pub block_hash: B256,
    /// The public key of the builder.
    pub builder_pubkey: BlsPublicKey,
    /// The public key of the proposer.
    pub proposer_pubkey: BlsPublicKey,
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

impl BidTrace {
    pub fn slot(&self) -> Slot {
        Slot::from(self.slot)
    }

    #[cfg(test)]
    pub fn random_for_test() -> Self {
        use crate::random_bls_pubkey;

        Self {
            slot: 0,
            parent_hash: B256::ZERO,
            block_hash: B256::ZERO,
            builder_pubkey: random_bls_pubkey(),
            proposer_pubkey: random_bls_pubkey(),
            proposer_fee_recipient: Address::ZERO,
            gas_limit: 0,
            gas_used: 0,
            value: U256::ZERO,
        }
    }
}

// TODO: refactor with superstruct?
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
#[serde(deny_unknown_fields)]
pub struct SignedBidSubmissionDeneb {
    pub message: BidTrace,
    pub execution_payload: ExecutionPayloadDeneb<MainnetEthSpec>,
    pub blobs_bundle: BlobsBundle,
    pub signature: BlsSignature,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
#[serde(deny_unknown_fields)]
pub struct SignedBidSubmissionElectra {
    pub message: BidTrace,
    pub execution_payload: ExecutionPayloadElectra<MainnetEthSpec>,
    pub blobs_bundle: BlobsBundle,
    pub execution_requests: ExecutionRequests,
    pub signature: BlsSignature,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
#[ssz(enum_behaviour = "transparent")]
#[tree_hash(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum SignedBidSubmission {
    Deneb(SignedBidSubmissionDeneb),
    Electra(SignedBidSubmissionElectra),
}

impl SignedBidSubmission {
    pub fn verify_signature(&self, spec: &ChainSpec) -> Result<(), SigError> {
        let domain = spec.get_builder_domain();
        let valid = match self {
            SignedBidSubmission::Deneb(bid) => {
                let message = bid.message.signing_root(domain);
                bid.signature.verify(&bid.message.builder_pubkey, message)
            }
            SignedBidSubmission::Electra(bid) => {
                let message = bid.message.signing_root(domain);
                bid.signature.verify(&bid.message.builder_pubkey, message)
            }
        };

        if !valid {
            return Err(SigError::InvalidBlsSignature);
        }

        Ok(())
    }

    pub fn num_txs(&self) -> usize {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.execution_payload.transactions.len()
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.execution_payload.transactions.len()
            }
        }
    }

    pub fn blobs_bundle(&self) -> &BlobsBundle {
        match &self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &signed_bid_submission.blobs_bundle
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                &signed_bid_submission.blobs_bundle
            }
        }
    }

    pub fn message(&self) -> &BidTrace {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => &signed_bid_submission.message,
            SignedBidSubmission::Deneb(signed_bid_submission) => &signed_bid_submission.message,
        }
    }

    pub fn message_mut(&mut self) -> &mut BidTrace {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                &mut signed_bid_submission.message
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => &mut signed_bid_submission.message,
        }
    }

    pub fn execution_payload(&self) -> ExecutionPayload {
        match self {
            SignedBidSubmission::Electra(signed_bid_submission) => {
                signed_bid_submission.execution_payload.clone().into()
            }
            SignedBidSubmission::Deneb(signed_bid_submission) => {
                signed_bid_submission.execution_payload.clone().into()
            }
        }
    }

    pub fn payload_and_blobs(&self) -> PayloadAndBlobs {
        PayloadAndBlobs {
            execution_payload: self.execution_payload().clone(),
            blobs_bundle: self.blobs_bundle().clone(),
        }
    }

    pub fn execution_requests(&self) -> Option<&ExecutionRequests> {
        match self {
            SignedBidSubmission::Deneb(_) => None,
            SignedBidSubmission::Electra(signed_bid_submission) => {
                Some(&signed_bid_submission.execution_requests)
            }
        }
    }
}

// TODO: SSZ tests
#[cfg(test)]
mod tests {
    use crate::signed_bid_submission::SignedBidSubmission;

    #[test]
    // from alloy
    fn deneb_bid_submission() {
        let s = r#"{"message":{"slot":"1","parent_hash":"0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2","block_hash":"0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2","builder_pubkey":"0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a", "proposer_pubkey": "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a","proposer_fee_recipient":"0xabcf8e0d4e9587369b2301d0790347320302cc09","gas_limit":"1","gas_used":"1","value":"1"},"execution_payload":{"parent_hash":"0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2","fee_recipient":"0xabcf8e0d4e9587369b2301d0790347320302cc09","state_root":"0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2","receipts_root":"0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2","logs_bloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","prev_randao":"0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2","block_number":"1","gas_limit":"1","gas_used":"1","timestamp":"1","extra_data":"0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2","base_fee_per_gas":"1","block_hash":"0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2","transactions":["0x02f878831469668303f51d843b9ac9f9843b9aca0082520894c93269b73096998db66be0441e836d873535cb9c8894a19041886f000080c001a031cc29234036afbf9a1fb9476b463367cb1f957ac0b919b69bbc798436e604aaa018c4e9c3914eb27aadd0b91e10b18655739fcf8c1fc398763a9f1beecb8ddc86"],"withdrawals":[{"index":"1","validator_index":"1","address":"0xabcf8e0d4e9587369b2301d0790347320302cc09","amount":"32000000000"}], "blob_gas_used":"1","excess_blob_gas":"1"},"blobs_bundle":{"commitments":[],"proofs":[],"blobs":[]},"signature":"0x88274f2d78d30ae429cc16f5c64657b491ccf26291c821cf953da34f16d60947d4f245decdce4a492e8d8f949482051b184aaa890d5dd97788387689335a1fee37cbe55c0227f81b073ce6e93b45f96169f497ed322d3d384d79ccaa7846d5ab"}"#;

        let bid = serde_json::from_str::<SignedBidSubmission>(s).unwrap();
        assert!(matches!(bid, SignedBidSubmission::Deneb(_)));
        let json: serde_json::Value = serde_json::from_str(s).unwrap();
        assert_eq!(json, serde_json::to_value(bid).unwrap());
    }

    #[test]
    // from alloy
    fn electra_bid_submission() {
        let s = include_str!("relay_builder_block_validation_request_v4.json");
        let bid = serde_json::from_str::<SignedBidSubmission>(s).unwrap();
        assert!(matches!(bid, SignedBidSubmission::Electra(_)));
        let json: serde_json::Value = serde_json::from_str(s).unwrap();
        assert_eq!(json, serde_json::to_value(bid).unwrap());
    }
}
