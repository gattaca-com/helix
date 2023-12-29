use ethereum_consensus::{
    primitives::{Root, Version},
    serde::{as_hex, as_str},
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct GenesisDetails {
    #[serde(with = "as_str")]
    pub genesis_time: u64,
    pub genesis_validators_root: Root,
    #[serde(with = "as_hex")]
    pub genesis_fork_version: Version,
}
