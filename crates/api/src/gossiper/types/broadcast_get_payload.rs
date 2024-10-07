use ethereum_consensus::{ssz, types::mainnet::SignedBlindedBeaconBlock};
use uuid::Uuid;

use crate::grpc;

#[derive(Clone, Debug)]
pub struct BroadcastGetPayloadParams {
    pub signed_blinded_beacon_block: SignedBlindedBeaconBlock,
    pub request_id: Uuid,
}

impl BroadcastGetPayloadParams {
    pub fn from_proto(proto_params: grpc::BroadcastGetPayloadParams) -> Self {
        Self {
            signed_blinded_beacon_block: ssz::prelude::deserialize(&proto_params.signed_blinded_beacon_block).unwrap(),
            request_id: Uuid::from_slice(&proto_params.request_id).unwrap(),
        }
    }
    pub fn to_proto(&self) -> grpc::BroadcastGetPayloadParams {
        grpc::BroadcastGetPayloadParams {
            signed_blinded_beacon_block: ssz::prelude::serialize(&self.signed_blinded_beacon_block).unwrap(),
            request_id: self.request_id.as_bytes().to_vec(),
        }
    }
}
