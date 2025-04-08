use helix_common::api::builder_api::TopBidUpdate;
use helix_types::{BlsPublicKey, SignedBuilderBid};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SignedBuilderBidWrapper {
    pub bid: SignedBuilderBid,
    pub slot: u64,
    pub builder_pub_key: BlsPublicKey,
    pub received_at_ms: u64,
}

impl SignedBuilderBidWrapper {
    pub fn new(
        bid: SignedBuilderBid,
        slot: u64,
        builder_pub_key: BlsPublicKey,
        received_at_ns: u128,
    ) -> Self {
        // convert received_at to millis, from nanos.
        let received_at_ms = (received_at_ns / 1_000_000) as u64;
        Self { bid, slot, builder_pub_key, received_at_ms }
    }
}

impl From<SignedBuilderBidWrapper> for TopBidUpdate {
    fn from(val: SignedBuilderBidWrapper) -> Self {
        TopBidUpdate {
            timestamp: val.received_at_ms,
            slot: val.slot,
            block_number: val.bid.message.header().block_number(),
            block_hash: val.bid.message.header().block_hash().0,
            parent_hash: val.bid.message.header().parent_hash().0,
            builder_pubkey: val.builder_pub_key,
            fee_recipient: val.bid.message.header().fee_recipient(),
            value: *val.bid.message.value(),
        }
    }
}
