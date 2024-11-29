use ethereum_consensus::primitives::BlsPublicKey;
use helix_common::{api::builder_api::TopBidUpdate, SignedBuilderBid};

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
        received_at: u128,
    ) -> Self {
        // convert received_at to millis, from nanos.
        let received_at = received_at / 1_000_000;

        Self { bid, slot, builder_pub_key, received_at_ms: received_at as u64 }
    }
}

impl From<SignedBuilderBidWrapper> for TopBidUpdate {
    fn from(val: SignedBuilderBidWrapper) -> Self {
        match val.bid {
            SignedBuilderBid::Bellatrix(bid, _) => TopBidUpdate {
                timestamp: val.received_at_ms,
                slot: val.slot,
                block_number: bid.message.header.block_number,
                block_hash: bid.message.header.block_hash,
                parent_hash: bid.message.header.parent_hash,
                builder_pubkey: val.builder_pub_key,
                fee_recipient: bid.message.header.fee_recipient,
                value: bid.message.value,
            },
            SignedBuilderBid::Capella(bid, _) => TopBidUpdate {
                timestamp: val.received_at_ms,
                slot: val.slot,
                block_number: bid.message.header.block_number,
                block_hash: bid.message.header.block_hash,
                parent_hash: bid.message.header.parent_hash,
                builder_pubkey: val.builder_pub_key,
                fee_recipient: bid.message.header.fee_recipient,
                value: bid.message.value,
            },
            SignedBuilderBid::Deneb(bid, _) => TopBidUpdate {
                timestamp: val.received_at_ms,
                slot: val.slot,
                block_number: bid.message.header.block_number,
                block_hash: bid.message.header.block_hash,
                parent_hash: bid.message.header.parent_hash,
                builder_pubkey: val.builder_pub_key,
                fee_recipient: bid.message.header.fee_recipient,
                value: bid.message.value,
            },
        }
    }
}
