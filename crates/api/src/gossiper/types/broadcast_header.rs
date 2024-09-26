use ethereum_consensus::{
    primitives::{BlsPublicKey, Hash32},
    ssz,
};
use helix_common::{bid_submission::BidTrace, SignedBuilderBid};

use crate::grpc;

#[derive(Clone, Debug)]
pub struct BroadcastHeaderParams {
    pub signed_builder_bid: SignedBuilderBid,
    pub bid_trace: BidTrace,
    pub slot: u64,
    pub parent_hash: Hash32,
    pub proposer_pub_key: BlsPublicKey,
    pub builder_pub_key: BlsPublicKey,
    pub is_cancellations_enabled: bool,
    pub on_receive: u64,
}

impl BroadcastHeaderParams {
    // TODO: impl SSZ serialisation for SignedBuilderBid instead of JSON
    pub fn from_proto(proto_params: grpc::BroadcastHeaderParams) -> Self {
        Self {
            signed_builder_bid: serde_json::from_slice(&proto_params.signed_builder_bid).unwrap(),
            bid_trace: ssz::prelude::deserialize(&proto_params.bid_trace).unwrap(),
            slot: proto_params.slot,
            parent_hash: Hash32::try_from(proto_params.parent_hash.as_slice()).unwrap(),
            proposer_pub_key: BlsPublicKey::try_from(proto_params.proposer_pub_key.as_slice()).unwrap(),
            builder_pub_key: BlsPublicKey::try_from(proto_params.builder_pub_key.as_slice()).unwrap(),
            is_cancellations_enabled: proto_params.is_cancellations_enabled,
            on_receive: proto_params.on_receive,
        }
    }

    // TODO: impl SSZ serialisation for SignedBuilderBid instead of JSON
    pub fn to_proto(&self) -> grpc::BroadcastHeaderParams {
        grpc::BroadcastHeaderParams {
            signed_builder_bid: serde_json::to_vec(&self.signed_builder_bid).unwrap(),
            bid_trace: ssz::prelude::serialize(&self.bid_trace).unwrap(),
            slot: self.slot,
            parent_hash: self.parent_hash.to_vec(),
            proposer_pub_key: self.proposer_pub_key.to_vec(),
            builder_pub_key: self.builder_pub_key.to_vec(),
            is_cancellations_enabled: self.is_cancellations_enabled,
            on_receive: self.on_receive,
        }
    }
}
