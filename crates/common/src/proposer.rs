use helix_types::{BlsPublicKey, SignedValidatorRegistration, Slot};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProposerDuty {
    #[serde(rename = "pubkey")]
    pub public_key: BlsPublicKey,
    #[serde(with = "serde_utils::quoted_u64")]
    pub validator_index: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub slot: u64,
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
    #[serde(rename = "pubkey")]
    pub pub_key: BlsPublicKey,
}
