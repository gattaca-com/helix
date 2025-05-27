use alloy_primitives::{bytes::Bytes, Address, B256, U256};
use helix_types::{BlsPublicKey, SignedValidatorRegistration, Slot, TestRandom};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};

use crate::{api::proposer_api::ValidatorRegistrationInfo, BuilderValidatorPreferences};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BuilderGetValidatorsResponseEntry {
    pub slot: Slot,
    #[serde(with = "serde_utils::quoted_u64")]
    pub validator_index: u64,
    pub entry: ValidatorRegistrationInfo,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BuilderGetValidatorsResponse {
    pub slot: Slot,
    #[serde(with = "serde_utils::quoted_u64")]
    pub validator_index: u64,
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

#[derive(Clone, Debug, Encode, Decode, TestRandom)]
pub struct TopBidUpdate {
    pub timestamp: u64,
    pub slot: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub parent_hash: B256,
    pub builder_pubkey: BlsPublicKey,
    pub fee_recipient: Address,
    pub value: U256,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct InclusionList {
    pub txs: Vec<InclusionListTx>,
}

impl InclusionList {
    pub const fn empty() -> Self {
        Self { txs: vec![] }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct InclusionListTx {
    pub hash: B256,
    pub nonce: u64,
    pub sender: Address,
    pub gas_priority_fee: u64,
    pub bytes: Bytes,
    pub wait_time: u32,
}
