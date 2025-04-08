use helix_types::{BlsPublicKey, ForkName, ForkVersionDecode, PayloadAndBlobs};
use ssz::Encode;

use crate::grpc;

#[derive(Clone, Debug)]
pub struct BroadcastPayloadParams {
    pub execution_payload: PayloadAndBlobs,
    pub slot: u64,
    pub proposer_pub_key: BlsPublicKey,
}

impl BroadcastPayloadParams {
    pub fn from_proto(proto_params: grpc::BroadcastPayloadParams, fork_name: ForkName) -> Self {
        // TODO: double check this logic
        Self {
            execution_payload: PayloadAndBlobs::from_ssz_bytes_by_fork(
                &proto_params.execution_payload,
                fork_name,
            )
            .unwrap(),
            slot: proto_params.slot,
            proposer_pub_key: BlsPublicKey::deserialize(proto_params.proposer_pub_key.as_slice())
                .unwrap(),
        }
    }
    pub fn to_proto(&self) -> grpc::BroadcastPayloadParams {
        grpc::BroadcastPayloadParams {
            execution_payload: self.execution_payload.as_ssz_bytes(),
            slot: self.slot,
            proposer_pub_key: self.proposer_pub_key.serialize().to_vec(),
        }
    }
}
