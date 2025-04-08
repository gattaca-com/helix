use alloy_primitives::B256;
use helix_types::BlsPublicKey;

use crate::grpc;

#[derive(Clone, Debug)]
pub struct RequestPayloadParams {
    pub slot: u64,
    pub proposer_pub_key: BlsPublicKey,
    pub block_hash: B256,
}

impl RequestPayloadParams {
    pub fn from_proto(proto_params: grpc::RequestPayloadParams) -> Self {
        Self {
            slot: proto_params.slot,
            proposer_pub_key: BlsPublicKey::deserialize(proto_params.proposer_pub_key.as_slice())
                .unwrap(),
            block_hash: B256::try_from(proto_params.block_hash.as_slice()).unwrap(),
        }
    }

    pub fn to_proto(&self) -> grpc::RequestPayloadParams {
        grpc::RequestPayloadParams {
            slot: self.slot,
            block_hash: self.block_hash.to_vec(),
            proposer_pub_key: self.proposer_pub_key.serialize().to_vec(),
        }
    }
}
