use ethereum_consensus::{
    builder::SignedValidatorRegistration,
    primitives::{BlsPublicKey, ExecutionAddress, Hash32, Slot, U256},
    serde::as_str,
    ssz::prelude::*,
};

use crate::{api::proposer_api::ValidatorRegistrationInfo, BuilderValidatorPreferences};

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
pub struct BuilderGetValidatorsResponseEntry {
    #[serde(with = "as_str")]
    pub slot: Slot,
    #[serde(with = "as_str")]
    pub validator_index: usize,
    pub entry: ValidatorRegistrationInfo,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
pub struct BuilderGetValidatorsResponse {
    #[serde(with = "as_str")]
    pub slot: Slot,
    #[serde(with = "as_str")]
    pub validator_index: usize,
    pub entry: SignedValidatorRegistration,
    pub preferences: BuilderValidatorPreferences,
}

impl From<BuilderGetValidatorsResponseEntry> for BuilderGetValidatorsResponse {
    fn from(entry: BuilderGetValidatorsResponseEntry) -> Self {
        Self {
            slot: entry.slot,
            validator_index: entry.validator_index,
            entry: entry.entry.registration,
            preferences: entry.entry.preferences.into(),
        }
    }
}

#[derive(Clone, Default, Debug, SimpleSerialize)]
pub struct TopBidUpdate {
    pub timestamp: u64,
    pub slot: u64,
    pub block_number: u64,
    pub block_hash: Hash32,
    pub parent_hash: Hash32,
    pub builder_pubkey: BlsPublicKey,
    pub fee_recipient: ExecutionAddress,
    pub value: U256,
}
