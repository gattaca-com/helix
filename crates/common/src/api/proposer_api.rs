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

/// Path params for Gloas's `getExecutionPayloadBid`. Adds `parent_root` relative to
/// `GetHeaderParams`: with ePBS the beacon block and execution payload are decoupled, so
/// `parent_hash` and `parent_root` may reference different parent blocks.
#[derive(Debug, serde::Deserialize, Clone, Copy)]
pub struct GetExecutionPayloadBidParams {
    pub slot: u64,
    pub parent_hash: B256,
    pub parent_root: B256,
    pub proposer_pubkey: BlsPublicKeyBytes,
}

/// Path params for Gloas's `submitBuilderPreferences`.
#[derive(Debug, serde::Deserialize, Clone, Copy)]
pub struct ProposerPubkeyParams {
    pub proposer_pubkey: BlsPublicKeyBytes,
}
