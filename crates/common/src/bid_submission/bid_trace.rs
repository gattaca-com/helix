use ethereum_consensus::{
    primitives::{BlsPublicKey, ExecutionAddress, Hash32, Slot, U256},
    serde::as_str, ssz::prelude::*, 
};

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct BidTrace {
    #[serde(with = "as_str")]
    pub slot: Slot,
    pub parent_hash: Hash32,
    pub block_hash: Hash32,
    #[serde(rename = "builder_pubkey")]
    pub builder_public_key: BlsPublicKey,
    #[serde(rename = "proposer_pubkey")]
    pub proposer_public_key: BlsPublicKey,
    pub proposer_fee_recipient: ExecutionAddress,
    #[serde(with = "as_str")]
    pub gas_limit: u64,
    #[serde(with = "as_str")]
    pub gas_used: u64,
    #[serde(with = "as_str")]
    pub value: U256,
}