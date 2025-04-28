use helix_types::{BlsPublicKey, SignedValidatorRegistration, Slot};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProposerDuty {
    pub pubkey: BlsPublicKey,
    #[serde(with = "serde_utils::quoted_u64")]
    pub validator_index: u64,
    pub slot: Slot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposerSchedule {
    pub slot: Slot,
    #[serde(with = "serde_utils::quoted_u64")]
    pub validator_index: u64,
    pub entry: SignedValidatorRegistration,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProposerInfo {
    pub name: String,
    pub pubkey: BlsPublicKey,
}
