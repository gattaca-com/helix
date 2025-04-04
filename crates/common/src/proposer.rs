use alloy_rpc_types::beacon::BlsPublicKey;
use ethereum_consensus::{
    builder::SignedValidatorRegistration,
    primitives::{Slot, ValidatorIndex},
    serde::as_str,
};
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
