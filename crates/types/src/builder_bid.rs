use std::sync::Arc;

use alloy_primitives::{B256, U256};
use serde::{Deserialize, Serialize};
use silver_ssz::ssz_hash::{compute_signing_root, hash_builder_bid_fulu};
use ssz::Encode;
use ssz_derive::{Decode, Encode};
use tree_hash_derive::TreeHash;

use crate::{
    BlsPublicKeyBytes, BlsSignatureBytes, ExecutionPayloadHeader, TestRandom,
    fields::{ExecutionRequests, KzgCommitments},
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
            blob_kzg_commitments: KzgCommitments::default(),
            execution_requests: ExecutionRequests::random_for_test(rng).into(),
            value: U256::random_for_test(rng),
            pubkey: BlsPublicKeyBytes::random(),
        }
    }
}

impl BuilderBid {
    pub fn signing_root(&self, domain: B256) -> B256 {
        let root = hash_builder_bid_fulu(&self.as_ssz_bytes());
        B256::from(compute_signing_root(&root, &domain.0))
    }
}

#[derive(PartialEq, Debug, Encode, Serialize, Deserialize, Clone)]
pub struct SignedBuilderBid {
    pub message: BuilderBid,
    pub signature: BlsSignatureBytes,
}

#[cfg(test)]
mod tests {
    use lh_types::{
        ConsolidationRequest, DepositRequest, ForkName, ForkVersionDecode, MainnetEthSpec,
        SigningData, WithdrawalRequest,
    };
    use ssz::Encode;
    use tree_hash::TreeHash;

    use super::*;
    use crate::{ExtraData, TestRandomSeed, fields::KzgCommitment};

    #[test]
    fn signing_root_matches_lighthouse() {
        let domain = B256::repeat_byte(3);
        for count in [0, 1, 6] {
            let mut bid = BuilderBid::test_random();
            bid.header.extra_data = ExtraData(vec![5; 17].into());
            let rng = &mut rand::rng();
            bid.execution_requests = Arc::new(ExecutionRequests {
                deposits: vec![DepositRequest::random_for_test(rng)].try_into().unwrap(),
                withdrawals: vec![WithdrawalRequest::random_for_test(rng)].try_into().unwrap(),
                consolidations: vec![ConsolidationRequest::random_for_test(rng)]
                    .try_into()
                    .unwrap(),
            });
            let commitments = (0..count).map(|_| KzgCommitment::random()).collect();
            bid.blob_kzg_commitments = KzgCommitments::new(commitments).unwrap();
            let lighthouse = lh_types::BuilderBid::<MainnetEthSpec>::from_ssz_bytes_by_fork(
                &bid.as_ssz_bytes(),
                ForkName::Fulu,
            )
            .unwrap();

            let object_root = lighthouse.tree_hash_root();
            let expected = SigningData { object_root, domain }.tree_hash_root();
            assert_eq!(bid.signing_root(domain), expected, "{count} commitments");
        }
    }

    #[test]
    fn test_execution_payload_header() {
        test_execution_payload_header_variant(ForkName::Fulu);
    }

    fn test_execution_payload_header_variant(fork_name: ForkName) {
        type LhBuilderBid = lh_types::BuilderBid<MainnetEthSpec>;

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
