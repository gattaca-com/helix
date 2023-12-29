use ethereum_consensus::{
    builder::SignedValidatorRegistration, primitives::Slot, serde::as_str,
};
use serde::{Deserialize, Serialize};

use super::proposer_api::{ValidatorPreferences, ValidatorRegistrationInfo};

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct BuilderGetValidatorsResponseEntry {
    #[serde(with = "as_str")]
    pub slot: Slot,
    #[serde(with = "as_str")]
    pub validator_index: usize,
    pub entry: ValidatorRegistrationInfo,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct BuilderGetValidatorsResponse {
    #[serde(with = "as_str")]
    pub slot: Slot,
    #[serde(with = "as_str")]
    pub validator_index: usize,
    pub entry: SignedValidatorRegistration,
    pub preferences: ValidatorPreferences,
}

impl From<BuilderGetValidatorsResponseEntry> for BuilderGetValidatorsResponse {
    fn from(entry: BuilderGetValidatorsResponseEntry) -> Self {
        Self {
            slot: entry.slot,
            validator_index: entry.validator_index,
            entry: entry.entry.registration,
            preferences: entry.entry.preferences,
        }
    }
}