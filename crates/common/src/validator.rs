use std::time::{SystemTime, UNIX_EPOCH};

use ethereum_consensus::{
    builder::{SignedValidatorRegistration, ValidatorRegistration},
    crypto::Signature,
    primitives::{
        ExecutionAddress,
        BlsPublicKey,
        Gwei,
        ValidatorIndex,
    },
    phase0::Validator,
    serde::as_str,
};
use reth_primitives::hex;
use serde::{Deserialize, Serialize};
use tokio_postgres::Row;

use crate::{ValidatorPreferences, api::proposer_api::ValidatorRegistrationInfo};


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
}

impl SignedValidatorRegistrationEntry {
    pub fn new(registration_info: ValidatorRegistrationInfo) -> Self {
        Self {
            registration_info,
            inserted_at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
        }
    }

    pub fn from_row(row: &Row) -> Self {
        let timestamp: i64 = row.get(2);
        let gas_limit: i64 = row.get(3);
        let inserted_at: i64 = row.get(6);
        Self {
            registration_info: ValidatorRegistrationInfo {
                registration: SignedValidatorRegistration {
                    message: ValidatorRegistration {
                        fee_recipient: string_to_execution_address(
                            row.try_get::<_, String>(1).unwrap(),
                        ),
                        timestamp: timestamp as u64,
                        gas_limit: gas_limit as u64,
                        public_key: string_to_bls_public_key(row.try_get::<_, String>(4).unwrap()),
                    },
                    signature: string_to_signature(row.try_get::<_, String>(5).unwrap()),
                },
                preferences: ValidatorPreferences::default(), // TODO: impl this table in postgres
            },
            inserted_at: inserted_at as u64,
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
