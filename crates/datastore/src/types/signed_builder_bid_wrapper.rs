use ethereum_consensus::primitives::BlsPublicKey;
use helix_common::{api::builder_api::TopBidUpdate, SignedBuilderBid};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SignedBuilderBidWrapper {
    pub bid: SignedBuilderBid,
    pub slot: u64,
    pub builder_pub_key: BlsPublicKey,
}

impl SignedBuilderBidWrapper {
    pub fn new(bid: SignedBuilderBid, slot: u64, builder_pub_key: BlsPublicKey) -> Self {
        Self {
            bid,
            slot,
            builder_pub_key,
        }
    }
}

impl Into<TopBidUpdate> for SignedBuilderBidWrapper {
    fn into(self) -> TopBidUpdate {
        match self.bid {
            SignedBuilderBid::Bellatrix(bid) => TopBidUpdate {
                timestamp: bid.message.header.timestamp,
                slot: self.slot,
                block_number: bid.message.header.block_number,
                block_hash: bid.message.header.block_hash,
                parent_hash: bid.message.header.parent_hash,
                builder_pubkey: self.builder_pub_key,
                fee_recipient: bid.message.header.fee_recipient,
                value: bid.message.value,
            },
            SignedBuilderBid::Capella(bid) => TopBidUpdate {
                timestamp: bid.message.header.timestamp,
                slot: self.slot,
                block_number: bid.message.header.block_number,
                block_hash: bid.message.header.block_hash,
                parent_hash: bid.message.header.parent_hash,
                builder_pubkey: self.builder_pub_key,
                fee_recipient: bid.message.header.fee_recipient,
                value: bid.message.value,
            },
            SignedBuilderBid::Deneb(bid) => TopBidUpdate {
                timestamp: bid.message.header.timestamp,
                slot: self.slot,
                block_number: bid.message.header.block_number,
                block_hash: bid.message.header.block_hash,
                parent_hash: bid.message.header.parent_hash,
                builder_pubkey: self.builder_pub_key,
                fee_recipient: bid.message.header.fee_recipient,
                value: bid.message.value,
            },
            
        }
    }

}