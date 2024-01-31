use ethereum_consensus::primitives::{BlsPublicKey, Hash32};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, Eq, PartialEq)]
pub struct PendingBlock {
    pub block_hash: Hash32,
    pub builder_pubkey: BlsPublicKey,
    pub slot: u64,
    pub header_receive_ms: Option<u64>,
    pub payload_receive_ms: Option<u64>,
}
