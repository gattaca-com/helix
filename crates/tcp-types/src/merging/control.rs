use alloy_primitives::Address;
use ssz_derive::{Decode, Encode};

use crate::Status;

pub const MAX_RELAY_ID_BYTES: usize = 64;
pub const MAX_ERROR_MSG_BYTES: usize = 256;
pub const MAX_BUILDER_COLLATERALS: usize = 256;

pub const TOTAL_BPS: u64 = 10_000;

/// First frame on a new connection, relay -> builder. The builder validates
/// `api_key` against its allowlist; on failure it drops the connection.
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct MergerRegistrationV1 {
    pub api_key: [u8; 16], // UUID bytes
    /// utf8 label for telemetry, <= MAX_RELAY_ID_BYTES.
    pub relay_id: Vec<u8>,
    pub min_version: u16,
    pub max_version: u16,
    pub supports_zstd: bool,
}

/// Builder -> relay handshake response.
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct MergerAckV1 {
    /// Chosen protocol version (highest common).
    pub version: u16,
    pub status: Status,
    pub error_msg: Vec<u8>,
    /// Advertised limits so the relay can pre-trim.
    pub max_orders_per_slot: u32,
    pub max_frame_bytes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Encode, Decode)]
pub struct PingV1 {
    pub nonce: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Encode, Decode)]
pub struct PongV1 {
    pub nonce: u64,
}

/// Relay-owned distribution policy. Required after ack; re-sendable at any
/// time, takes effect at the next `SlotStartV1`.
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct RelayConfigV1 {
    pub relay_fee_recipient: Address,
    pub multisend_contract: Address,
    pub relay_bps: u64,
    pub merged_builder_bps: u64,
    pub winning_builder_bps: u64,
    /// Gas reserved for the distribution tx (140_000 today).
    pub distribution_gas_limit: u64,
    pub builder_collaterals: Vec<BuilderCollateral>,
}

#[derive(Debug, Clone, Copy, PartialEq, Encode, Decode)]
pub struct BuilderCollateral {
    /// Base-block fee recipient.
    pub builder_coinbase: Address,
    /// Safe the multisend pays from.
    pub collateral_safe: Address,
}

#[derive(Debug, thiserror::Error)]
pub enum RelayConfigError {
    #[error("bps sum {0} >= {TOTAL_BPS}")]
    InvalidBps(u64),
    #[error("empty builder collaterals")]
    EmptyCollaterals,
    #[error("too many builder collaterals: {0}")]
    TooManyCollaterals(usize),
}

impl RelayConfigV1 {
    /// The proposer receives `TOTAL_BPS - sum` of merged revenue; the sum must
    /// leave it non-zero.
    pub fn validate(&self) -> Result<(), RelayConfigError> {
        let sum = self.relay_bps + self.merged_builder_bps + self.winning_builder_bps;
        if sum >= TOTAL_BPS {
            return Err(RelayConfigError::InvalidBps(sum));
        }
        if self.builder_collaterals.is_empty() {
            return Err(RelayConfigError::EmptyCollaterals);
        }
        if self.builder_collaterals.len() > MAX_BUILDER_COLLATERALS {
            return Err(RelayConfigError::TooManyCollaterals(self.builder_collaterals.len()));
        }
        Ok(())
    }

    pub fn collateral_safe(&self, builder_coinbase: &Address) -> Option<Address> {
        self.builder_collaterals
            .iter()
            .find(|c| c.builder_coinbase == *builder_coinbase)
            .map(|c| c.collateral_safe)
    }
}
