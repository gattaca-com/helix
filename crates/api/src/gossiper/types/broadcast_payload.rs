use ethereum_consensus::{primitives::BlsPublicKey, ssz};
use helix_common::versioned_payload::PayloadAndBlobs;

use crate::grpc;

#[derive(Clone, Debug)]
pub struct BroadcastPayloadParams {
    pub execution_payload: PayloadAndBlobs,
    pub slot: u64,
    pub proposer_pub_key: BlsPublicKey,
}

impl BroadcastPayloadParams {
    pub fn from_proto(proto_params: grpc::BroadcastPayloadParams) -> Self {
        Self {
            execution_payload: ssz::prelude::deserialize(&proto_params.execution_payload).unwrap(),
            slot: proto_params.slot,
            proposer_pub_key: BlsPublicKey::try_from(proto_params.proposer_pub_key.as_slice()).unwrap(),
        }
    }
    pub fn to_proto(&self) -> grpc::BroadcastPayloadParams {
        grpc::BroadcastPayloadParams {
            execution_payload: ssz::prelude::serialize(&self.execution_payload).unwrap(),
            slot: self.slot,
            proposer_pub_key: self.proposer_pub_key.to_vec(),
        }
    }
}
