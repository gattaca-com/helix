use std::time::Duration;

use alloy_primitives::B256;
use helix_types::{
    custom_slot_clock, duration_into_slot, holesky_slot_clock, holesky_spec, mainnet_slot_clock,
    sepolia_slot_clock, sepolia_spec, spec_from_file, ChainSpec, EthSpec, ForkName, MainnetEthSpec,
    Slot, SlotClock, SlotClockTrait, HOLESKY_GENESIS_TIME, MAINNET_GENESIS_TIME,
    SEPOLIA_GENESIS_TIME,
};

pub(crate) const MAINNET_GENESIS_VALIDATOR_ROOT: [u8; 32] = [
    75, 54, 61, 185, 78, 40, 97, 32, 215, 110, 185, 5, 52, 15, 221, 78, 84, 191, 233, 240, 107,
    243, 63, 246, 207, 90, 210, 127, 81, 27, 254, 149,
];
pub(crate) const SEPOLIA_GENESIS_VALIDATOR_ROOT: [u8; 32] = [
    216, 234, 23, 31, 60, 148, 174, 162, 30, 188, 66, 161, 237, 97, 5, 42, 207, 63, 146, 9, 192,
    14, 78, 251, 170, 221, 172, 9, 237, 155, 128, 120,
];
pub(crate) const HOLESKY_GENESIS_VALIDATOR_ROOT: [u8; 32] = [
    145, 67, 170, 124, 97, 90, 127, 113, 21, 226, 182, 170, 195, 25, 192, 53, 41, 223, 130, 66,
    174, 112, 95, 186, 157, 243, 155, 121, 197, 159, 168, 177,
];

#[derive(Default, Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    #[default]
    Mainnet,
    Sepolia,
    Holesky,
    Custom(String),
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mainnet => write!(f, "mainnet"),
            Self::Sepolia => write!(f, "sepolia"),
            Self::Holesky => write!(f, "holesky"),
            Self::Custom(config) => write!(f, "custom network with config at `{config}`"),
        }
    }
}

/// Runtime config with all chain specific information
#[derive(Clone)]
pub struct ChainInfo {
    pub network: Network,
    pub genesis_validators_root: B256,
    // TODO: load this from beacon on startup?
    pub context: ChainSpec,
    pub clock: SlotClock,
    // TODO: remove?
    pub genesis_time_in_secs: u64,
}

impl ChainInfo {
    pub fn for_mainnet() -> Self {
        let context = ChainSpec::mainnet();
        Self {
            network: Network::Mainnet,
            genesis_validators_root: B256::from(MAINNET_GENESIS_VALIDATOR_ROOT),
            clock: mainnet_slot_clock(context.seconds_per_slot),
            context,
            genesis_time_in_secs: MAINNET_GENESIS_TIME,
        }
    }

    pub fn for_sepolia() -> Self {
        let context = sepolia_spec();
        Self {
            network: Network::Sepolia,
            genesis_validators_root: B256::from(SEPOLIA_GENESIS_VALIDATOR_ROOT),
            clock: sepolia_slot_clock(context.seconds_per_slot),
            context,
            genesis_time_in_secs: SEPOLIA_GENESIS_TIME,
        }
    }

    pub fn for_holesky() -> Self {
        let context = holesky_spec();
        Self {
            network: Network::Holesky,
            genesis_validators_root: B256::from(HOLESKY_GENESIS_VALIDATOR_ROOT),
            clock: holesky_slot_clock(context.seconds_per_slot),
            context,
            genesis_time_in_secs: HOLESKY_GENESIS_TIME,
        }
    }

    pub fn for_custom(
        config: String,
        genesis_validators_root: B256,
        genesis_time_in_secs: u64,
    ) -> Self {
        let context = spec_from_file(&config);
        let network = Network::Custom(config.clone());
        let clock = custom_slot_clock(genesis_time_in_secs, context.seconds_per_slot);

        Self { network, genesis_validators_root, context, clock, genesis_time_in_secs }
    }

    pub fn current_fork_name(&self) -> ForkName {
        let current_slot = self.clock.now().unwrap();
        self.context.fork_name_at_slot::<MainnetEthSpec>(current_slot)
    }

    pub fn seconds_per_slot(&self) -> u64 {
        self.context.seconds_per_slot
    }

    pub fn slots_per_epoch(&self) -> u64 {
        MainnetEthSpec::slots_per_epoch()
    }

    /// Returns the position of the slot in the epoch, 0-31.
    pub fn slot_in_epoch(&self, slot: Slot) -> u64 {
        slot.as_u64() % self.slots_per_epoch()
    }

    /// Duration since the start of the slot, None if we're before the start of the slot
    pub fn duration_into_slot(&self, slot: Slot) -> Option<Duration> {
        duration_into_slot(&self.clock, slot)
    }

    /// Current slot based on current clock time
    pub fn current_slot(&self) -> Slot {
        // safe since we're past genesis slot and UNIX_EPOCH
        self.clock.now().unwrap()
    }
}
