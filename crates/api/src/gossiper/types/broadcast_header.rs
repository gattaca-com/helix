use alloy_primitives::B256;
use helix_common::bid_submission::v3::header_submission_v3::PayloadSocketAddress;
use helix_types::{BidTrace, BlsPublicKey, SignedBuilderBid};
use ssz::{Decode, Encode};

use crate::grpc;

#[derive(Clone, Debug)]
pub struct BroadcastHeaderParams {
    pub signed_builder_bid: SignedBuilderBid,
    pub bid_trace: BidTrace,
    pub slot: u64,
    pub parent_hash: B256,
    pub proposer_pub_key: BlsPublicKey,
    pub builder_pub_key: BlsPublicKey,
    pub is_cancellations_enabled: bool,
    pub on_receive: u64,
    pub payload_address: Option<PayloadSocketAddress>,
}

impl BroadcastHeaderParams {
    // TODO: impl SSZ serialisation for SignedBuilderBid instead of JSON
    pub fn from_proto(proto_params: grpc::BroadcastHeaderParams) -> Self {
        Self {
            signed_builder_bid: serde_json::from_slice(&proto_params.signed_builder_bid).unwrap(),
            bid_trace: BidTrace::from_ssz_bytes(&proto_params.bid_trace).unwrap(),
            slot: proto_params.slot,
            parent_hash: B256::try_from(proto_params.parent_hash.as_slice()).unwrap(),
            proposer_pub_key: BlsPublicKey::deserialize(proto_params.proposer_pub_key.as_slice())
                .unwrap(),
            builder_pub_key: BlsPublicKey::deserialize(proto_params.builder_pub_key.as_slice())
                .unwrap(),
            is_cancellations_enabled: proto_params.is_cancellations_enabled,
            on_receive: proto_params.on_receive,
            payload_address: proto_params
                .payload_address
                .map(|vec| PayloadSocketAddress::from_ssz_bytes(&vec).unwrap()),
        }
    }

    // TODO: impl SSZ serialisation for SignedBuilderBid instead of JSON
    pub fn to_proto(&self) -> grpc::BroadcastHeaderParams {
        grpc::BroadcastHeaderParams {
            signed_builder_bid: serde_json::to_vec(&self.signed_builder_bid).unwrap(),
            bid_trace: BidTrace::as_ssz_bytes(&self.bid_trace),
            slot: self.slot,
            parent_hash: self.parent_hash.to_vec(),
            proposer_pub_key: self.proposer_pub_key.serialize().to_vec(),
            builder_pub_key: self.builder_pub_key.serialize().to_vec(),
            is_cancellations_enabled: self.is_cancellations_enabled,
            on_receive: self.on_receive,
            payload_address: self.payload_address.as_ref().map(PayloadSocketAddress::as_ssz_bytes),
        }
    }
}
