use ethereum_consensus::{
    builder::SignedValidatorRegistration,
    primitives::{BlsPublicKey, Slot, ValidatorIndex},
    serde::as_str,
};
use reth_primitives::revm_primitives::{HashMap, HashSet};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProposerDuty {
    #[serde(rename = "pubkey")]
    pub public_key: BlsPublicKey,
    #[serde(with = "as_str")]
    pub validator_index: ValidatorIndex,
    #[serde(with = "as_str")]
    pub slot: Slot,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProposerSchedule {
    #[serde(with = "as_str")]
    pub slot: Slot,
    #[serde(with = "as_str")]
    pub validator_index: ValidatorIndex,
    pub entry: SignedValidatorRegistration,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProposerInfo {
    pub name: String,
    #[serde(rename = "pubkey")]
    pub pub_key: BlsPublicKey,
}

impl From<Vec<ProposerInfo>> for ProposerInfoSet {
    fn from(proposer_infos: Vec<ProposerInfo>) -> Self {
        ProposerInfoSet(proposer_infos.into_iter().map(|info| info.pub_key).collect())
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProposerInfoSet(HashSet<BlsPublicKey>);

impl ProposerInfoSet {
    pub fn contains(&self, public_key: &BlsPublicKey) -> bool {
        self.0.contains(public_key)
    }
}