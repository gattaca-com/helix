use std::time::{SystemTime, UNIX_EPOCH};

use ethereum_consensus::{
    crypto::Signature,
    phase0::Validator,
    primitives::{BlsPublicKey, ExecutionAddress, Gwei, ValidatorIndex},
    serde::as_str,
};
use reth_primitives::hex;
use serde::{Deserialize, Serialize};

use crate::api::proposer_api::ValidatorRegistrationInfo;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ValidatorSummary {
    #[serde(with = "as_str")]
    pub index: ValidatorIndex,
    #[serde(with = "as_str")]
    pub balance: Gwei,
    pub status: ValidatorStatus,
    pub validator: Validator,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorStatus {
    PendingInitialized,
    PendingQueued,
    ActiveOngoing,
    ActiveExiting,
    ActiveSlashed,
    ExitedUnslashed,
    ExitedSlashed,
    WithdrawalPossible,
    WithdrawalDone,
    Active,
    Pending,
    Exited,
    Withdrawal,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SignedValidatorRegistrationEntry {
    pub registration_info: ValidatorRegistrationInfo,
    pub inserted_at: u64,
    pub pool_name: Option<String>,
}

impl SignedValidatorRegistrationEntry {
    pub fn new(registration_info: ValidatorRegistrationInfo, pool_name: Option<String>) -> Self {
        Self {
            registration_info,
            inserted_at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
            pool_name,
        }
    }

    pub fn public_key(&self) -> &BlsPublicKey {
        &self.registration_info.registration.message.public_key
    }
}

fn string_to_execution_address(s: String) -> ExecutionAddress {
    let bytes = hex::decode(s).unwrap();
    ExecutionAddress::try_from(bytes.as_slice()).unwrap()
}

fn string_to_bls_public_key(s: String) -> BlsPublicKey {
    let bytes = hex::decode(s).unwrap();
    BlsPublicKey::try_from(bytes.as_slice()).unwrap()
}

fn string_to_signature(s: String) -> Signature {
    let bytes = hex::decode(s).unwrap();
    Signature::try_from(bytes.as_slice()).unwrap()
}
