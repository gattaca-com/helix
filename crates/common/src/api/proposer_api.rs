use alloy_primitives::B256;
use helix_types::{BlsPublicKeyBytes, SignedValidatorRegistration};

use crate::validator_preferences::ValidatorPreferences;

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ValidatorRegistrationInfo {
    pub registration: SignedValidatorRegistration,
    pub preferences: ValidatorPreferences,
}

#[derive(Debug, serde::Deserialize, Clone, Copy)]
pub struct GetHeaderParams {
    pub slot: u64,
    pub parent_hash: B256,
    pub pubkey: BlsPublicKeyBytes,
}
