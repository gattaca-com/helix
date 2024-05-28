

use ethereum_consensus::{ssz, types::mainnet::SignedBlindedBeaconBlock};

use crate::grpc;

#[derive(Clone, Debug)]
pub struct BroadcastGetPayloadParams {
    pub signed_blinded_beacon_block: SignedBlindedBeaconBlock,
}

impl BroadcastGetPayloadParams {
    pub fn from_proto(proto_params: grpc::BroadcastGetPayloadParams) -> Self {
        Self {
            signed_blinded_beacon_block: ssz::prelude::deserialize(&proto_params.signed_blinded_beacon_block).unwrap(),
        }
    }
    pub fn to_proto(&self) -> grpc::BroadcastGetPayloadParams {
        grpc::BroadcastGetPayloadParams {
            signed_blinded_beacon_block: ssz::prelude::serialize(&self.signed_blinded_beacon_block).unwrap(),
        }
    }
}
