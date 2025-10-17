use std::collections::HashMap;

use alloy_primitives::{Address, U256};
use helix_types::MergeableOrderRecovered;
use reth_ethereum::provider::ProviderError;
use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, serde_as};

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BlockMergingConfig {
    /// Builder coinbase -> collateral signer. The base block coinbase will accrue fees and
    /// disperse from its collateral address
    pub builder_collateral_map: HashMap<Address, PrivateKeySigner>,
    /// Address for the relay's share of fees
    pub relay_fee_recipient: Address,
    /// Configuration for revenue distribution.
    pub distribution_config: DistributionConfig,
    /// Address of disperse contract.
    /// It must have a `disperseEther(address[],uint256[])` function.
    pub disperse_address: Address,
    /// Whether to validate merged blocks or not
    pub validate_merged_blocks: bool,
}

#[serde_as]
#[derive(Debug, Clone, Deserialize)]
/// Wrapper over [`alloy_signer_local::PrivateKeySigner`], that implements [`serde::Deserialize`]
pub(crate) struct PrivateKeySigner(
    #[serde_as(as = "DisplayFromStr")] pub(crate) alloy_signer_local::PrivateKeySigner,
);

/// Configuration for revenue distribution among different parties.
/// The total basis points is 10000, which means each participant
/// will be paid `revenue * x / 10000`.
///
/// This config does not have an explicit proposer allocation.
/// It is assumed it will be the remaining bps not allocated
/// to other parties.
#[derive(Debug, Serialize, Eq, PartialEq, Deserialize, Clone)]
pub(crate) struct DistributionConfig {
    /// Base points allocated to the relay.
    pub(crate) relay_bps: u64,
    /// Base points allocated to the builder that sent the bundle.
    pub(crate) merged_builder_bps: u64,
    /// Base points allocated to the winning builder.
    pub(crate) winning_builder_bps: u64,
}

impl Default for DistributionConfig {
    fn default() -> Self {
        let relay_bps = Self::TOTAL_BPS / 4;
        let merged_builder_bps = Self::TOTAL_BPS / 4;
        let winning_builder_bps = Self::TOTAL_BPS / 4;

        Self { relay_bps, merged_builder_bps, winning_builder_bps }
    }
}

impl DistributionConfig {
    const TOTAL_BPS: u64 = 10000;

    pub(crate) fn validate(&self) {
        assert!(
            self.relay_bps + self.winning_builder_bps + self.merged_builder_bps < Self::TOTAL_BPS,
            "invalid distribution config, sum of bps exceeds 10000"
        );
    }

    pub(crate) fn split(&self, bps: u64, revenue: U256) -> U256 {
        (U256::from(bps) * revenue) / U256::from(Self::TOTAL_BPS)
    }

    pub(crate) fn relay_split(&self, revenue: U256) -> U256 {
        self.split(self.relay_bps, revenue)
    }

    pub(crate) fn proposer_split(&self, revenue: U256) -> U256 {
        let proposer_bps =
            Self::TOTAL_BPS - self.relay_bps - self.merged_builder_bps - self.winning_builder_bps;
        self.split(proposer_bps, revenue)
    }

    pub(crate) fn merged_builder_split(&self, revenue: U256) -> U256 {
        self.split(self.merged_builder_bps, revenue)
    }
}

pub(crate) struct SimulatedOrder {
    pub(crate) order: MergeableOrderRecovered,
    pub(crate) gas_used: u64,
    pub(crate) should_be_included: Vec<bool>,
    pub(crate) builder_payment: U256,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SimulationError {
    #[error("tx decode error: {_0}")]
    Provider(#[from] ProviderError),
    #[error("zero builder payment")]
    ZeroBuilderPayment,
    #[error("gas used exceeds allotted block limit")]
    OutOfBlockGas,
    #[error("blobs used exceed allotted block limit")]
    OutOfBlockBlobs,
    #[error("duplicate transaction in bundle")]
    DuplicateTransaction,
    #[error("transaction {_0} reverted and is not allowed to revert")]
    RevertNotAllowed(usize),
    #[error("transaction {_0} is invalid and can't be dropped")]
    DropNotAllowed(usize),
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    #[test]
    fn serde_builder_collateral_map() {
        let json = include_str!("../../builders.json.example");
        let deserialized_map: HashMap<Address, PrivateKeySigner> =
            serde_json::from_str(json).unwrap();

        // We compare with the address of each private key in the builder collateral map
        let expected_private_key_addresses = HashMap::from([
            (
                address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"), // anvil address 0
                address!("0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"), // anvil address 5
            ),
            (
                address!("0x70997970C51812dc3A010C7d01b50e0d17dc79C8"), // anvil address 1
                address!("0x976EA74026E726554dB657fA54763abd0C3a0aa9"), // anvil address 6
            ),
            (
                address!("0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"), // anvil address 2
                address!("0x14dC79964da2C08b23698B3D3cc7Ca32193d9955"), // anvil address 7
            ),
        ]);

        for (address, expected_address) in expected_private_key_addresses {
            let private_key =
                deserialized_map.get(&address).expect(&format!("address {address} is missing"));
            assert_eq!(private_key.0.address(), expected_address);
        }
    }
}
