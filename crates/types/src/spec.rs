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

pub fn spec_from_file<P: AsRef<Path>>(path: P) -> ChainSpec {
    let file = std::fs::File::open(path).unwrap();
    let config: lh_types::Config = serde_json::from_reader(file).unwrap();
    ChainSpec::from_config::<MainnetEthSpec>(&config).unwrap()
}
