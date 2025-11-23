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
        // Sepolia Altair fork happened at epoch 50
        assert_eq!(spec.altair_fork_epoch, Some(lh_types::Epoch::new(50)), "Sepolia Altair fork at epoch 50");
    }

    #[test]
    fn test_holesky_spec() {
        let spec = holesky_spec();
        assert_eq!(spec.seconds_per_slot, 12);
        // Holesky launched post-merge with Altair already active
        assert_eq!(spec.altair_fork_epoch, Some(lh_types::Epoch::new(0)), "Holesky started at Altair fork");
    }

    #[test]
    fn test_hoodi_spec() {
        let spec = hoodi_spec();
        assert_eq!(spec.seconds_per_slot, 12);
        // Hoodi is a modern test network with all forks active from genesis
        assert_eq!(spec.altair_fork_epoch, Some(lh_types::Epoch::new(0)), "Hoodi started at Altair fork");
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

        let sepolia_domain = sepolia.get_builder_domain();
        let holesky_domain = holesky.get_builder_domain();
        let hoodi_domain = hoodi.get_builder_domain();

        // Domains must be non-zero (uniqueness test doesn't guarantee this)
        use alloy_primitives::B256;
        assert_ne!(sepolia_domain, B256::ZERO, "Sepolia builder domain should not be zero");
        assert_ne!(holesky_domain, B256::ZERO, "Holesky builder domain should not be zero");
        assert_ne!(hoodi_domain, B256::ZERO, "Hoodi builder domain should not be zero");
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
    fn test_genesis_fork_versions_are_non_zero() {
        let sepolia = sepolia_spec();
        let holesky = holesky_spec();
        let hoodi = hoodi_spec();

        // Genesis fork versions must not be zero (uniqueness doesn't guarantee this)
        assert_ne!(sepolia.genesis_fork_version, [0u8; 4], "Sepolia genesis fork version should not be zero");
        assert_ne!(holesky.genesis_fork_version, [0u8; 4], "Holesky genesis fork version should not be zero");
        assert_ne!(hoodi.genesis_fork_version, [0u8; 4], "Hoodi genesis fork version should not be zero");
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



}
