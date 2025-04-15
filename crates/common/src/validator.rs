use helix_types::{BlsPublicKey, Validator};
use serde::{Deserialize, Serialize};

use crate::{api::proposer_api::ValidatorRegistrationInfo, utils::utcnow_ms};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ValidatorSummary {
    #[serde(with = "serde_utils::quoted_u64")]
    pub index: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub balance: u64,
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SignedValidatorRegistrationEntry {
    pub registration_info: ValidatorRegistrationInfo,
    pub inserted_at: u64,
    pub pool_name: Option<String>,
    pub user_agent: Option<String>,
}

impl SignedValidatorRegistrationEntry {
    pub fn new(
        registration_info: ValidatorRegistrationInfo,
        pool_name: Option<String>,
        user_agent: Option<String>,
    ) -> Self {
        Self { registration_info, inserted_at: utcnow_ms(), pool_name, user_agent }
    }

    pub fn public_key(&self) -> &BlsPublicKey {
        &self.registration_info.registration.message.pubkey
    }
}
