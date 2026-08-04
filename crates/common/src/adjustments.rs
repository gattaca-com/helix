use alloy_primitives::{Address, B256, U256, address};
use chrono::{DateTime, Utc};
use helix_types::{BlsPublicKeyBytes, Slot};
use serde::{Deserialize, Serialize};

/// `PaymentForwarder`, see contracts/README.md. A payment routed through it is
/// only valid in the block it was signed for.
pub const PAYMENT_FORWARDER: Address = address!("0xFEEEEEECC8AdE925fA6099f017712A04b5546A32");

/// Recipient encoded in the leading bytes of a [`PAYMENT_FORWARDER`] call, or
/// `None` if the calldata is too short to be one.
pub fn payment_forwarder_recipient(input: &[u8]) -> Option<Address> {
    (input.len() >= 28).then(|| Address::from_slice(&input[..20]))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarder_recipient() {
        let recipient = address!("0x1234560000000000000000000000000000000abc");
        let mut input = recipient.to_vec();
        input.extend_from_slice(&1_900_000_000u64.to_be_bytes());
        assert_eq!(payment_forwarder_recipient(&input), Some(recipient));

        input.extend_from_slice(b"inner calldata");
        assert_eq!(payment_forwarder_recipient(&input), Some(recipient));

        assert_eq!(payment_forwarder_recipient(&input[..27]), None);
        assert_eq!(payment_forwarder_recipient(&[]), None);
    }
}
