use std::time::Duration;

use alloy_primitives::B256;
use helix_types::{
    ChainSpec, EthSpec, ForkName, HOLESKY_GENESIS_TIME, HOODI_GENESIS_TIME, MAINNET_GENESIS_TIME,
    MainnetEthSpec, SEPOLIA_GENESIS_TIME, Slot, SlotClock, SlotClockTrait, custom_slot_clock,
    duration_into_slot, holesky_slot_clock, holesky_spec, hoodi_slot_clock, hoodi_spec,
    mainnet_slot_clock, sepolia_slot_clock, sepolia_spec, spec_from_file,
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

pub(crate) const HOODI_GENESIS_VALIDATOR_ROOT: [u8; 32] = [
    33, 47, 19, 252, 77, 240, 120, 182, 203, 125, 178, 40, 241, 200, 48, 117, 102, 220, 236, 249,
    0, 134, 116, 1, 169, 32, 35, 215, 186, 153, 203, 95,
];

#[derive(Default, Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    #[default]
    Mainnet,
    Sepolia,
    Holesky,
    Hoodi,
    Custom(String),
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mainnet => write!(f, "mainnet"),
            Self::Sepolia => write!(f, "sepolia"),
            Self::Holesky => write!(f, "holesky"),
            Self::Hoodi => write!(f, "hoodi"),
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
    pub builder_domain: B256,
}

impl ChainInfo {
    pub fn for_mainnet() -> Self {
        let context = ChainSpec::mainnet();
        let builder_domain = context.get_builder_domain();
        Self {
            network: Network::Mainnet,
            genesis_validators_root: B256::from(MAINNET_GENESIS_VALIDATOR_ROOT),
            clock: mainnet_slot_clock(context.seconds_per_slot),
            context,
            genesis_time_in_secs: MAINNET_GENESIS_TIME,
            builder_domain,
        }
    }

    pub fn for_sepolia() -> Self {
        let context = sepolia_spec();
        let builder_domain = context.get_builder_domain();
        Self {
            network: Network::Sepolia,
            genesis_validators_root: B256::from(SEPOLIA_GENESIS_VALIDATOR_ROOT),
            clock: sepolia_slot_clock(context.seconds_per_slot),
            context,
            genesis_time_in_secs: SEPOLIA_GENESIS_TIME,
            builder_domain,
        }
    }

    pub fn for_holesky() -> Self {
        let context = holesky_spec();
        let builder_domain = context.get_builder_domain();
        Self {
            network: Network::Holesky,
            genesis_validators_root: B256::from(HOLESKY_GENESIS_VALIDATOR_ROOT),
            clock: holesky_slot_clock(context.seconds_per_slot),
            context,
            genesis_time_in_secs: HOLESKY_GENESIS_TIME,
            builder_domain,
        }
    }

    pub fn for_hoodi() -> Self {
        let context = hoodi_spec();
        let builder_domain = context.get_builder_domain();
        Self {
            network: Network::Hoodi,
            genesis_validators_root: B256::from(HOODI_GENESIS_VALIDATOR_ROOT),
            clock: hoodi_slot_clock(context.seconds_per_slot),
            context,
            genesis_time_in_secs: HOODI_GENESIS_TIME,
            builder_domain,
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
        let builder_domain = context.get_builder_domain();
        Self {
            network,
            genesis_validators_root,
            context,
            clock,
            genesis_time_in_secs,
            builder_domain,
        }
    }

    pub fn fork_at_slot(&self, slot: Slot) -> ForkName {
        self.context.fork_name_at_slot::<MainnetEthSpec>(slot)
    }

    pub fn current_fork_name(&self) -> ForkName {
        let current_slot = self.clock.now().unwrap();
        self.fork_at_slot(current_slot)
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

    pub fn max_blobs_per_block(&self) -> usize {
        let epoch = self.current_slot().epoch(self.slots_per_epoch());
        self.context.max_blobs_per_block(epoch) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_display() {
        assert_eq!(format!("{}", Network::Mainnet), "mainnet");
        assert_eq!(format!("{}", Network::Sepolia), "sepolia");
        assert_eq!(format!("{}", Network::Holesky), "holesky");
        assert_eq!(format!("{}", Network::Hoodi), "hoodi");
        assert_eq!(
            format!("{}", Network::Custom("/path/to/config.yaml".to_string())),
            "custom network with config at `/path/to/config.yaml`"
        );
    }

    #[test]
    fn test_network_default() {
        let network = Network::default();
        assert!(matches!(network, Network::Mainnet));
    }

    #[test]
    fn test_network_serialization() {
        let network = Network::Sepolia;
        let serialized = serde_json::to_string(&network).unwrap();
        let deserialized: Network = serde_json::from_str(&serialized).unwrap();
        assert!(matches!(deserialized, Network::Sepolia));
    }

    #[test]
    fn test_chain_info_for_mainnet() {
        let chain_info = ChainInfo::for_mainnet();
        assert!(matches!(chain_info.network, Network::Mainnet));
        assert_eq!(chain_info.genesis_time_in_secs, MAINNET_GENESIS_TIME);
        assert_eq!(chain_info.genesis_validators_root, B256::from(MAINNET_GENESIS_VALIDATOR_ROOT));
        assert_eq!(chain_info.seconds_per_slot(), 12);
    }

    #[test]
    fn test_chain_info_for_sepolia() {
        let chain_info = ChainInfo::for_sepolia();
        assert!(matches!(chain_info.network, Network::Sepolia));
        assert_eq!(chain_info.genesis_time_in_secs, SEPOLIA_GENESIS_TIME);
        assert_eq!(chain_info.genesis_validators_root, B256::from(SEPOLIA_GENESIS_VALIDATOR_ROOT));
        assert_eq!(chain_info.seconds_per_slot(), 12);
    }

    #[test]
    fn test_chain_info_for_holesky() {
        let chain_info = ChainInfo::for_holesky();
        assert!(matches!(chain_info.network, Network::Holesky));
        assert_eq!(chain_info.genesis_time_in_secs, HOLESKY_GENESIS_TIME);
        assert_eq!(chain_info.genesis_validators_root, B256::from(HOLESKY_GENESIS_VALIDATOR_ROOT));
        assert_eq!(chain_info.seconds_per_slot(), 12);
    }

    #[test]
    fn test_chain_info_for_hoodi() {
        let chain_info = ChainInfo::for_hoodi();
        assert!(matches!(chain_info.network, Network::Hoodi));
        assert_eq!(chain_info.genesis_time_in_secs, HOODI_GENESIS_TIME);
        assert_eq!(chain_info.genesis_validators_root, B256::from(HOODI_GENESIS_VALIDATOR_ROOT));
        assert_eq!(chain_info.seconds_per_slot(), 12);
    }

    #[test]
    fn test_chain_info_slots_per_epoch() {
        let chain_info = ChainInfo::for_mainnet();
        assert_eq!(chain_info.slots_per_epoch(), 32);
    }

    #[test]
    fn test_chain_info_slot_in_epoch() {
        let chain_info = ChainInfo::for_mainnet();
        
        // Test slot 0 should be at position 0
        assert_eq!(chain_info.slot_in_epoch(Slot::new(0)), 0);
        
        // Test slot 31 should be at position 31 (last in epoch)
        assert_eq!(chain_info.slot_in_epoch(Slot::new(31)), 31);
        
        // Test slot 32 should be at position 0 (first in next epoch)
        assert_eq!(chain_info.slot_in_epoch(Slot::new(32)), 0);
        
        // Test slot 64 should be at position 0 (first in epoch 2)
        assert_eq!(chain_info.slot_in_epoch(Slot::new(64)), 0);
        
        // Test arbitrary slot
        assert_eq!(chain_info.slot_in_epoch(Slot::new(100)), 4); // 100 % 32 = 4
    }

    #[test]
    fn test_chain_info_current_slot() {
        let chain_info = ChainInfo::for_mainnet();
        let current_slot = chain_info.current_slot();
        
        // Current slot should be a reasonable value (not 0, since we're past genesis)
        assert!(current_slot.as_u64() > 0);
    }

    #[test]
    fn test_chain_info_current_fork_name() {
        let chain_info = ChainInfo::for_mainnet();
        let fork_name = chain_info.current_fork_name();
        
        // Should return some fork name
        assert!(matches!(
            fork_name,
            ForkName::Base | ForkName::Altair | ForkName::Bellatrix | 
            ForkName::Capella | ForkName::Deneb | ForkName::Electra | ForkName::Fulu
        ));
    }

    #[test]
    fn test_chain_info_fork_at_slot() {
        let chain_info = ChainInfo::for_mainnet();
        
        // Slot 0 should be in Base fork
        let fork_at_genesis = chain_info.fork_at_slot(Slot::new(0));
        assert_eq!(fork_at_genesis, ForkName::Base);
    }

    #[test]
    fn test_chain_info_max_blobs_per_block() {
        let chain_info = ChainInfo::for_mainnet();
        let max_blobs = chain_info.max_blobs_per_block();
        
        // Should return a positive number (6 for mainnet after Deneb)
        assert!(max_blobs > 0);
        assert!(max_blobs <= 6);
    }

    #[test]
    fn test_genesis_validator_roots_are_unique() {
        let mainnet_root = B256::from(MAINNET_GENESIS_VALIDATOR_ROOT);
        let sepolia_root = B256::from(SEPOLIA_GENESIS_VALIDATOR_ROOT);
        let holesky_root = B256::from(HOLESKY_GENESIS_VALIDATOR_ROOT);
        let hoodi_root = B256::from(HOODI_GENESIS_VALIDATOR_ROOT);

        assert_ne!(mainnet_root, sepolia_root);
        assert_ne!(mainnet_root, holesky_root);
        assert_ne!(mainnet_root, hoodi_root);
        assert_ne!(sepolia_root, holesky_root);
        assert_ne!(sepolia_root, hoodi_root);
        assert_ne!(holesky_root, hoodi_root);
    }

    #[test]
    fn test_genesis_times_are_unique_and_reasonable() {
        assert!(MAINNET_GENESIS_TIME > 1_600_000_000); // After 2020
        assert!(SEPOLIA_GENESIS_TIME > MAINNET_GENESIS_TIME);
        assert!(HOLESKY_GENESIS_TIME > SEPOLIA_GENESIS_TIME);
        assert!(HOODI_GENESIS_TIME > HOLESKY_GENESIS_TIME);
    }
}
