use ethereum_consensus::{
    builder::SignedValidatorRegistration,
    primitives::{BlsPublicKey, Root, Slot, ValidatorIndex, Version},
    serde::{as_hex, as_str},
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
