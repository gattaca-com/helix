use std::path::Path;

use lh_types::MainnetEthSpec;

pub type ChainSpec = lh_types::ChainSpec;

pub fn sepolia_spec() -> ChainSpec {
    let spec = include_bytes!("specs/sepolia_spec.json");
    let config: lh_types::Config = serde_json::from_slice(spec).unwrap();
    ChainSpec::from_config::<MainnetEthSpec>(&config).unwrap()
}

pub fn holesky_spec() -> ChainSpec {
    let spec = include_bytes!("specs/holesky_spec.json");
    let config: lh_types::Config = serde_json::from_slice(spec).unwrap();
    ChainSpec::from_config::<MainnetEthSpec>(&config).unwrap()
}

pub fn hoodi_spec() -> ChainSpec {
    let spec = include_bytes!("specs/hoodi_spec.json");
    let config: lh_types::Config = serde_json::from_slice(spec).unwrap();
    ChainSpec::from_config::<MainnetEthSpec>(&config).unwrap()
}

pub fn spec_from_file<P: AsRef<Path>>(path: P) -> ChainSpec {
    let file = std::fs::File::open(path).unwrap();
    let config: lh_types::Config = serde_json::from_reader(file).unwrap();
    ChainSpec::from_config::<MainnetEthSpec>(&config).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sepolia_spec() {
        let spec = sepolia_spec();
        assert_eq!(spec.seconds_per_slot, 12);
        // Sepolia specific checks
        assert!(spec.altair_fork_epoch.is_some());
    }

    #[test]
    fn test_holesky_spec() {
        let spec = holesky_spec();
        assert_eq!(spec.seconds_per_slot, 12);
        // Holesky specific checks
        assert!(spec.altair_fork_epoch.is_some());
    }

    #[test]
    fn test_hoodi_spec() {
        let spec = hoodi_spec();
        assert_eq!(spec.seconds_per_slot, 12);
        // Hoodi specific checks
        assert!(spec.altair_fork_epoch.is_some());
    }

    #[test]
    fn test_specs_have_different_genesis_fork_versions() {
        let sepolia = sepolia_spec();
        let holesky = holesky_spec();
        let hoodi = hoodi_spec();

        // Each network should have unique genesis fork version
        assert_ne!(sepolia.genesis_fork_version, holesky.genesis_fork_version);
        assert_ne!(sepolia.genesis_fork_version, hoodi.genesis_fork_version);
        assert_ne!(holesky.genesis_fork_version, hoodi.genesis_fork_version);
    }

    #[test]
    fn test_specs_have_builder_domain() {
        let sepolia = sepolia_spec();
        let holesky = holesky_spec();
        let hoodi = hoodi_spec();

        // All specs should be able to compute builder domain
        let sepolia_domain = sepolia.get_builder_domain();
        let holesky_domain = holesky.get_builder_domain();
        let hoodi_domain = hoodi.get_builder_domain();

        // Domains should be non-zero
        use alloy_primitives::B256;
        assert_ne!(sepolia_domain, B256::ZERO);
        assert_ne!(holesky_domain, B256::ZERO);
        assert_ne!(hoodi_domain, B256::ZERO);
    }

    #[test]
    fn test_specs_have_fork_schedule() {
        let sepolia = sepolia_spec();
        let holesky = holesky_spec();
        let hoodi = hoodi_spec();

        // All should have altair fork
        assert!(sepolia.altair_fork_epoch.is_some());
        assert!(holesky.altair_fork_epoch.is_some());
        assert!(hoodi.altair_fork_epoch.is_some());

        // All should have bellatrix fork
        assert!(sepolia.bellatrix_fork_epoch.is_some());
        assert!(holesky.bellatrix_fork_epoch.is_some());
        assert!(hoodi.bellatrix_fork_epoch.is_some());
    }

    #[test]
    fn test_all_specs_use_mainnet_eth_spec() {
        let sepolia = sepolia_spec();
        let holesky = holesky_spec();
        let hoodi = hoodi_spec();

        // All test networks should use 32 slots per epoch like mainnet
        use lh_types::EthSpec;
        assert_eq!(MainnetEthSpec::slots_per_epoch(), 32);
        
        // Verify they use standard slot timing
        assert_eq!(sepolia.seconds_per_slot, 12);
        assert_eq!(holesky.seconds_per_slot, 12);
        assert_eq!(hoodi.seconds_per_slot, 12);
    }

    #[test]
    fn test_specs_have_unique_builder_domains() {
        let sepolia = sepolia_spec();
        let holesky = holesky_spec();
        let hoodi = hoodi_spec();

        let sepolia_domain = sepolia.get_builder_domain();
        let holesky_domain = holesky.get_builder_domain();
        let hoodi_domain = hoodi.get_builder_domain();

        // Each network must have unique builder domain (security: prevents replay attacks)
        assert_ne!(sepolia_domain, holesky_domain, "Sepolia and Holesky must have different builder domains");
        assert_ne!(sepolia_domain, hoodi_domain, "Sepolia and Hoodi must have different builder domains");
        assert_ne!(holesky_domain, hoodi_domain, "Holesky and Hoodi must have different builder domains");
    }

    #[test]
    fn test_fork_epochs_are_ordered() {
        let sepolia = sepolia_spec();
        
        // Forks should be ordered chronologically (each fork happens after previous)
        if let (Some(altair), Some(bellatrix)) = (sepolia.altair_fork_epoch, sepolia.bellatrix_fork_epoch) {
            assert!(bellatrix >= altair, "Bellatrix should come after Altair");
        }
        
        if let (Some(bellatrix), Some(capella)) = (sepolia.bellatrix_fork_epoch, sepolia.capella_fork_epoch) {
            assert!(capella >= bellatrix, "Capella should come after Bellatrix");
        }
        
        if let (Some(capella), Some(deneb)) = (sepolia.capella_fork_epoch, sepolia.deneb_fork_epoch) {
            assert!(deneb >= capella, "Deneb should come after Capella");
        }
    }

    #[test]
    fn test_spec_loading_is_consistent() {
        // Multiple loads should return identical specs
        let spec1 = sepolia_spec();
        let spec2 = sepolia_spec();
        
        assert_eq!(spec1.seconds_per_slot, spec2.seconds_per_slot);
        assert_eq!(spec1.genesis_fork_version, spec2.genesis_fork_version);
        assert_eq!(spec1.altair_fork_epoch, spec2.altair_fork_epoch);
        
        // Same for other networks
        let holesky1 = holesky_spec();
        let holesky2 = holesky_spec();
        assert_eq!(holesky1.genesis_fork_version, holesky2.genesis_fork_version);
    }

    #[test]
    fn test_all_specs_have_reasonable_slot_timing() {
        let sepolia = sepolia_spec();
        let holesky = holesky_spec();
        let hoodi = hoodi_spec();

        // Slot time should be reasonable (between 1 and 60 seconds)
        for spec in [sepolia, holesky, hoodi] {
            assert!(spec.seconds_per_slot > 0, "Slot time must be positive");
            assert!(spec.seconds_per_slot <= 60, "Slot time should be <= 60 seconds");
            assert_eq!(spec.seconds_per_slot, 12, "Ethereum uses 12-second slots");
        }
    }

    #[test]
    fn test_genesis_fork_versions_are_non_zero() {
        let sepolia = sepolia_spec();
        let holesky = holesky_spec();
        let hoodi = hoodi_spec();

        // Genesis fork versions should not be all zeros
        assert_ne!(sepolia.genesis_fork_version, [0u8; 4], "Sepolia genesis fork version should not be zero");
        assert_ne!(holesky.genesis_fork_version, [0u8; 4], "Holesky genesis fork version should not be zero");
        assert_ne!(hoodi.genesis_fork_version, [0u8; 4], "Hoodi genesis fork version should not be zero");
    }

    #[test]
    fn test_specs_have_post_merge_forks() {
        let sepolia = sepolia_spec();
        let holesky = holesky_spec();
        let hoodi = hoodi_spec();

        // All test networks should have Capella (post-merge withdrawal support)
        assert!(sepolia.capella_fork_epoch.is_some(), "Sepolia should have Capella fork");
        assert!(holesky.capella_fork_epoch.is_some(), "Holesky should have Capella fork");
        assert!(hoodi.capella_fork_epoch.is_some(), "Hoodi should have Capella fork");

        // All should have Deneb (blob support)
        assert!(sepolia.deneb_fork_epoch.is_some(), "Sepolia should have Deneb fork");
        assert!(holesky.deneb_fork_epoch.is_some(), "Holesky should have Deneb fork");
        assert!(hoodi.deneb_fork_epoch.is_some(), "Hoodi should have Deneb fork");
    }

    #[test]
    fn test_builder_domain_computation_is_deterministic() {
        // Builder domain should be same for same spec
        let spec1 = sepolia_spec();
        let spec2 = sepolia_spec();
        
        let domain1 = spec1.get_builder_domain();
        let domain2 = spec2.get_builder_domain();
        
        assert_eq!(domain1, domain2, "Builder domain should be deterministic");
    }
}
