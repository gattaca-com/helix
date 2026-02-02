use alloy_primitives::{B256, U256};
use chrono::{DateTime, Utc};
use helix_types::{BlsPublicKeyBytes, Slot};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct DataAdjustmentsEntry {
    pub slot: Slot,
    pub builder_pubkey: BlsPublicKeyBytes,
    pub block_number: u64,
    pub delta: U256,
    pub submitted_block_hash: B256,
    pub submitted_received_at: DateTime<Utc>,
    pub submitted_value: U256,
    pub adjusted_block_hash: B256,
    pub adjusted_value: U256,
    pub is_dry_run: bool,
    pub metadata: serde_json::Value,
}
