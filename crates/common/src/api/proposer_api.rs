use helix_types::SignedValidatorRegistration;

use crate::validator_preferences::ValidatorPreferences;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ValidatorRegistrationInfo {
    pub registration: SignedValidatorRegistration,
    pub preferences: ValidatorPreferences,
}
