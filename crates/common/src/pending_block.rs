use alloy_primitives::B256;
use helix_types::BlsPublicKey;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Eq, PartialEq)]
pub struct PendingBlock {
    pub block_hash: B256,
    pub builder_pubkey: BlsPublicKey,
    pub slot: u64,
    pub header_receive_ms: Option<u64>,
    pub payload_receive_ms: Option<u64>,
}
