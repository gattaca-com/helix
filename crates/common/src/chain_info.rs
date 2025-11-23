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
    fn test_current_slot_is_past_genesis() {
        let chain_info = ChainInfo::for_mainnet();
        let current_slot = chain_info.current_slot();
        
        // Mainnet launched Dec 2020, by Nov 2024 should have > 9M slots
        // (4 years * 365 days * 7200 slots/day ≈ 10.5M)
        assert!(current_slot.as_u64() > 9_000_000, "Mainnet should have > 9M slots by now, got {}", current_slot.as_u64());
        
        // Sanity check - should not be absurdly high
        assert!(current_slot.as_u64() < 50_000_000, "Slot count seems too high: {}", current_slot.as_u64());
    }

    #[test]
    fn test_chain_info_current_fork_name() {
        let chain_info = ChainInfo::for_mainnet();
        let fork_name = chain_info.current_fork_name();
        
        // By Nov 2024, mainnet should be at Deneb or later
        assert!(
            matches!(fork_name, ForkName::Deneb | ForkName::Electra | ForkName::Fulu),
            "Mainnet should be at Deneb or later by Nov 2024, got {:?}",
            fork_name
        );
    }

    #[test]
    fn test_chain_info_fork_at_slot() {
        let chain_info = ChainInfo::for_mainnet();
        
        // Slot 0 should be in Base fork
        let fork_at_genesis = chain_info.fork_at_slot(Slot::new(0));
        assert_eq!(fork_at_genesis, ForkName::Base);
    }

    #[test]
    fn test_max_blobs_is_positive_and_reasonable() {
        // Each network may have different max_blobs based on their fork schedule
        // Deneb introduced 6 blobs per block, future forks may increase
        
        let mainnet_blobs = ChainInfo::for_mainnet().max_blobs_per_block();
        assert!(mainnet_blobs > 0, "Mainnet should support blobs (post-Deneb), got {}", mainnet_blobs);
        assert!(mainnet_blobs <= 128, "Mainnet max blobs should be reasonable, got {}", mainnet_blobs);
        
        let sepolia_blobs = ChainInfo::for_sepolia().max_blobs_per_block();
        assert!(sepolia_blobs > 0, "Sepolia should support blobs, got {}", sepolia_blobs);
        assert!(sepolia_blobs <= 128, "Sepolia max blobs should be reasonable, got {}", sepolia_blobs);
        
        let holesky_blobs = ChainInfo::for_holesky().max_blobs_per_block();
        assert!(holesky_blobs > 0, "Holesky should support blobs, got {}", holesky_blobs);
        assert!(holesky_blobs <= 128, "Holesky max blobs should be reasonable, got {}", holesky_blobs);
        
        let hoodi_blobs = ChainInfo::for_hoodi().max_blobs_per_block();
        assert!(hoodi_blobs > 0, "Hoodi should support blobs, got {}", hoodi_blobs);
        assert!(hoodi_blobs <= 128, "Hoodi max blobs should be reasonable, got {}", hoodi_blobs);
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

    #[test]
    fn test_slot_in_epoch_edge_cases() {
        let chain_info = ChainInfo::for_mainnet();
        let slots_per_epoch = chain_info.slots_per_epoch();
        assert_eq!(slots_per_epoch, 32, "Mainnet should have 32 slots per epoch");

        // Epoch boundaries
        assert_eq!(chain_info.slot_in_epoch(Slot::new(0)), 0, "First slot of epoch 0");
        assert_eq!(chain_info.slot_in_epoch(Slot::new(31)), 31, "Last slot of epoch 0");
        assert_eq!(chain_info.slot_in_epoch(Slot::new(32)), 0, "First slot of epoch 1");
        assert_eq!(chain_info.slot_in_epoch(Slot::new(63)), 31, "Last slot of epoch 1");
        assert_eq!(chain_info.slot_in_epoch(Slot::new(64)), 0, "First slot of epoch 2");

        // Large slot numbers
        let large_slot = Slot::new(1_000_000);
        let position = chain_info.slot_in_epoch(large_slot);
        assert!(position < 32, "Position should be < 32, got {}", position);
        assert_eq!(position, 1_000_000 % 32);

        // Very large slot (near u64 max, but safe for calculation)
        let very_large = Slot::new(u64::MAX - 100);
        let position = chain_info.slot_in_epoch(very_large);
        assert!(position < 32, "Should handle very large slots");
    }

    #[test]
    fn test_builder_domain_unique_per_network() {
        let mainnet = ChainInfo::for_mainnet();
        let sepolia = ChainInfo::for_sepolia();
        let holesky = ChainInfo::for_holesky();
        let hoodi = ChainInfo::for_hoodi();

        // Each network must have a unique builder domain to prevent replay attacks
        assert_ne!(mainnet.builder_domain, sepolia.builder_domain, 
                   "Mainnet and Sepolia must have different builder domains");
        assert_ne!(mainnet.builder_domain, holesky.builder_domain,
                   "Mainnet and Holesky must have different builder domains");
        assert_ne!(mainnet.builder_domain, hoodi.builder_domain,
                   "Mainnet and Hoodi must have different builder domains");
        assert_ne!(sepolia.builder_domain, holesky.builder_domain,
                   "Sepolia and Holesky must have different builder domains");
        assert_ne!(sepolia.builder_domain, hoodi.builder_domain,
                   "Sepolia and Hoodi must have different builder domains");
        assert_ne!(holesky.builder_domain, hoodi.builder_domain,
                   "Holesky and Hoodi must have different builder domains");

        // Builder domains should not be zero (security check)
        assert_ne!(mainnet.builder_domain, B256::ZERO, "Builder domain should not be zero");
        assert_ne!(sepolia.builder_domain, B256::ZERO, "Builder domain should not be zero");
    }

    #[test]
    fn test_network_serialization_edge_cases() {
        // Standard networks
        let networks = vec![
            Network::Mainnet,
            Network::Sepolia,
            Network::Holesky,
            Network::Hoodi,
        ];

        for network in networks {
            let serialized = serde_json::to_string(&network).unwrap();
            let deserialized: Network = serde_json::from_str(&serialized).unwrap();
            // Can't use assert_eq! because Network doesn't derive PartialEq
            let reserialized = serde_json::to_string(&deserialized).unwrap();
            assert_eq!(serialized, reserialized, "Round trip should be identical");
        }

        // Custom network with various path formats
        let custom_paths = vec![
            "/absolute/path/to/config.yaml",
            "relative/path/config.yaml",
            "./local/config.yaml",
            "../parent/config.yaml",
            "/path/with spaces/config.yaml",
            "/path/with-special_chars!@#/config.yaml",
        ];

        for path in custom_paths {
            let network = Network::Custom(path.to_string());
            let serialized = serde_json::to_string(&network).unwrap();
            let deserialized: Network = serde_json::from_str(&serialized).unwrap();
            
            if let Network::Custom(deserialized_path) = deserialized {
                assert_eq!(deserialized_path, path, "Custom path should round-trip");
            } else {
                panic!("Expected Network::Custom, got {:?}", deserialized);
            }
        }
    }

    #[test]
    fn test_network_display_edge_cases() {
        // Empty custom path
        let empty_custom = Network::Custom(String::new());
        let display = format!("{}", empty_custom);
        assert!(display.contains("custom network"));
        assert!(display.contains("``"), "Empty path should show as empty backticks");

        // Very long path
        let long_path = "a".repeat(1000);
        let long_custom = Network::Custom(long_path.clone());
        let display = format!("{}", long_custom);
        assert!(display.contains(&long_path), "Should display full long path");

        // Path with unicode
        let unicode_path = "/path/to/配置文件.yaml";
        let unicode_custom = Network::Custom(unicode_path.to_string());
        let display = format!("{}", unicode_custom);
        assert!(display.contains(unicode_path), "Should preserve Unicode in path");
    }

    #[test]
    fn test_all_networks_have_consistent_slot_timing() {
        let mainnet = ChainInfo::for_mainnet();
        let sepolia = ChainInfo::for_sepolia();
        let holesky = ChainInfo::for_holesky();
        let hoodi = ChainInfo::for_hoodi();

        // All Ethereum networks use 12-second slots
        assert_eq!(mainnet.seconds_per_slot(), 12, "Mainnet should use 12s slots");
        assert_eq!(sepolia.seconds_per_slot(), 12, "Sepolia should use 12s slots");
        assert_eq!(holesky.seconds_per_slot(), 12, "Holesky should use 12s slots");
        assert_eq!(hoodi.seconds_per_slot(), 12, "Hoodi should use 12s slots");

        // All use 32 slots per epoch
        assert_eq!(mainnet.slots_per_epoch(), 32);
        assert_eq!(sepolia.slots_per_epoch(), 32);
        assert_eq!(holesky.slots_per_epoch(), 32);
        assert_eq!(hoodi.slots_per_epoch(), 32);
    }

    #[test]
    fn test_fork_at_slot_genesis() {
        // Different networks started at different forks
        let mainnet = ChainInfo::for_mainnet();
        assert_eq!(mainnet.fork_at_slot(Slot::new(0)), ForkName::Base,
                   "Mainnet genesis should be Base fork");

        let sepolia = ChainInfo::for_sepolia();
        assert_eq!(sepolia.fork_at_slot(Slot::new(0)), ForkName::Base,
                   "Sepolia genesis should be Base fork");

        // Holesky launched later, starting at Bellatrix (The Merge)
        let holesky = ChainInfo::for_holesky();
        assert_eq!(holesky.fork_at_slot(Slot::new(0)), ForkName::Bellatrix,
                   "Holesky genesis should be Bellatrix fork (launched post-merge)");

        // Hoodi also launched post-merge
        let hoodi = ChainInfo::for_hoodi();
        let hoodi_genesis_fork = hoodi.fork_at_slot(Slot::new(0));
        // Hoodi should be at Bellatrix or later
        assert!(
            matches!(hoodi_genesis_fork, ForkName::Bellatrix | ForkName::Capella | ForkName::Deneb | ForkName::Electra | ForkName::Fulu),
            "Hoodi should start at Bellatrix or later, got {:?}", hoodi_genesis_fork
        );
    }
}
