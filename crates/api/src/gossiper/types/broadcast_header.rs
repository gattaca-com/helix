use alloy_primitives::B256;
use helix_types::{BidTrace, BlsPublicKey, SignedBuilderBid, Slot};
use ssz::{Decode, Encode};

use crate::grpc;

#[derive(Clone, Debug)]
pub struct BroadcastHeaderParams {
    pub signed_builder_bid: SignedBuilderBid,
    pub bid_trace: BidTrace,
    pub is_cancellations_enabled: bool,
    pub on_receive: u64,
    pub payload_address: Option<Vec<u8>>,
}

impl BroadcastHeaderParams {
    pub fn slot(&self) -> Slot {
        self.bid_trace.slot()
    }
    pub fn parent_hash(&self) -> &B256 {
        &self.bid_trace.parent_hash
    }
    pub fn proposer_pubkey(&self) -> &BlsPublicKey {
        &self.bid_trace.proposer_pubkey
    }
    pub fn builder_pubkey(&self) -> &BlsPublicKey {
        &self.bid_trace.builder_pubkey
    }
}

impl BroadcastHeaderParams {
    // TODO: impl SSZ serialisation for SignedBuilderBid instead of JSON
    pub fn from_proto(proto_params: grpc::BroadcastHeaderParams) -> Self {
        Self {
            signed_builder_bid: serde_json::from_slice(&proto_params.signed_builder_bid).unwrap(),
            bid_trace: BidTrace::from_ssz_bytes(&proto_params.bid_trace).unwrap(),
            is_cancellations_enabled: proto_params.is_cancellations_enabled,
            on_receive: proto_params.on_receive,
            payload_address: proto_params.payload_address,
        }
    }

    // TODO: impl SSZ serialisation for SignedBuilderBid instead of JSON
    pub fn to_proto(&self) -> grpc::BroadcastHeaderParams {
        grpc::BroadcastHeaderParams {
            signed_builder_bid: serde_json::to_vec(&self.signed_builder_bid).unwrap(),
            bid_trace: BidTrace::as_ssz_bytes(&self.bid_trace),
            is_cancellations_enabled: self.is_cancellations_enabled,
            on_receive: self.on_receive,
            payload_address: self.payload_address.clone(),
        }
    }
}
