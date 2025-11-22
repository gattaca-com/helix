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
        assert_ne!(sepolia_domain, Default::default());
        assert_ne!(holesky_domain, Default::default());
        assert_ne!(hoodi_domain, Default::default());
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
}
