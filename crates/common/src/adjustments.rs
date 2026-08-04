use alloy_primitives::{Address, B256, U256, address, b256};
use chrono::{DateTime, Utc};
use helix_types::{BlsPublicKeyBytes, Slot};
use serde::{Deserialize, Serialize};

/// `PaymentForwarder`, see contracts/README.md. A payment routed through it is
/// only valid in the block it was signed for.
pub const PAYMENT_FORWARDER: Address = address!("0xFEEEEEE44046c3f61a8CC081E0918eF0de0a7ffC");

/// Runtime deployed at [`PAYMENT_FORWARDER`]. A value call to that address on a
/// chain without it succeeds while crediting the address itself, so a payment is
/// only recognised where the runtime is present.
pub const PAYMENT_FORWARDER_CODE_HASH: B256 =
    b256!("0xd9f5db49d3c0a174c39701485406cd78d01dd27f73b7ef7b883e5f69d8103220");

/// Recipient in a [`PAYMENT_FORWARDER`] call, whose calldata is a 4 byte
/// timestamp followed by the 20 byte recipient.
#[inline(always)]
pub fn payment_forwarder_recipient(input: &[u8]) -> Option<Address> {
    (input.len() >= 24).then(|| Address::from_slice(&input[4..24]))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
