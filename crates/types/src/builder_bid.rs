use std::sync::Arc;

use alloy_primitives::U256;
use lh_types::{test_utils::TestRandom, SignedRoot};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

use crate::{
    fields::{ExecutionRequests, KzgCommitments},
    BlsPublicKeyBytes, BlsSignatureBytes, ExecutionPayloadHeader,
};

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
pub struct BuilderBid {
    pub header: ExecutionPayloadHeader,
    pub blob_kzg_commitments: KzgCommitments,
    pub execution_requests: Arc<ExecutionRequests>,
    #[serde(with = "serde_utils::quoted_u256")]
    pub value: U256,
    pub pubkey: BlsPublicKeyBytes,
}

impl TestRandom for BuilderBid {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            header: ExecutionPayloadHeader::random_for_test(rng),
            blob_kzg_commitments: KzgCommitments::random_for_test(rng),
            execution_requests: ExecutionRequests::random_for_test(rng).into(),
            value: U256::random_for_test(rng),
            pubkey: BlsPublicKeyBytes::random(),
        }
    }
}

impl SignedRoot for BuilderBid {}

#[derive(PartialEq, Debug, Encode, Serialize, Deserialize, Clone)]
pub struct SignedBuilderBid {
    pub message: BuilderBid,
    pub signature: BlsSignatureBytes,
}

#[cfg(test)]
mod tests {
    use lh_types::{ForkName, ForkVersionDecode, MainnetEthSpec};
    use ssz::Encode;
    use tree_hash::TreeHash;

    use crate::{BuilderBid, TestRandomSeed};

    #[test]
    fn test_execution_payload_header() {
        test_execution_payload_header_variant(ForkName::Electra);
        test_execution_payload_header_variant(ForkName::Fulu);
    }

    fn test_execution_payload_header_variant(fork_name: ForkName) {
        type LhBuilderBid = lh_types::builder_bid::BuilderBid<MainnetEthSpec>;

        let our_payload = BuilderBid::test_random();
        let ssz_bytes = our_payload.as_ssz_bytes();

        // ssz
        let lh_decode = LhBuilderBid::from_ssz_bytes_by_fork(&ssz_bytes, fork_name).unwrap();
        assert_eq!(our_payload.tree_hash_root(), lh_decode.tree_hash_root());

        // serde
        let json_str = serde_json::to_value(&our_payload).unwrap();
        let lh_json_str: LhBuilderBid = serde_json::from_value(json_str).unwrap();
        assert_eq!(our_payload.tree_hash_root(), lh_json_str.tree_hash_root());
    }
}
