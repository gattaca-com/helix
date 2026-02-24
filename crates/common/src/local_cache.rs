use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use alloy_primitives::B256;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use dashmap::{DashMap, DashSet};
use helix_types::{BlsPublicKeyBytes, CryptoError, MergedBlock, SignedValidatorRegistration};
use parking_lot::RwLock;
use rustc_hash::FxHashSet;
use tracing::error;

use crate::{
    BuilderConfig, BuilderInfo, SignedValidatorRegistrationEntry,
    api::{
        builder_api::{
            BuilderGetValidatorsResponseEntry, InclusionListWithKey, InclusionListWithMetadata,
            SlotCoordinate,
        },
        proposer_api::ValidatorRegistrationInfo,
    },
    metrics::CACHE_SIZE,
};

const ESTIMATED_BUILDER_INFOS_UPPER_BOUND: usize = 1000;
const MAX_PRIMEV_PROPOSERS: usize = 64;
const VALIDATOR_REGISTRATION_UPDATE_INTERVAL: u64 = 60 * 60; // 1 hour in seconds

#[derive(Debug, thiserror::Error)]
pub enum AuctioneerError {
    #[error("unexpected value type")]
    UnexpectedValueType,

    #[error("crypto error: {0:?}")]
    CryptoError(CryptoError),

    #[error("from utf8 error: {0}")]
    FromUtf8Error(#[from] std::string::FromUtf8Error),

    #[error("parse int error: {0}")]
    ParseIntError(#[from] std::num::ParseIntError),

    #[error("from hex error: {0}")]
    FromHexError(#[from] alloy_primitives::hex::FromHexError),

    #[error("past slot already delivered")]
    PastSlotAlreadyDelivered,

    #[error("another payload already delivered for slot")]
    AnotherPayloadAlreadyDeliveredForSlot,

    #[error("ssz deserialize error: {0:?}")]
    SszDeserializeError(ssz::DecodeError),

    #[error("Slice conversion error: {0:?}")]
    SliceConversionError(#[from] core::array::TryFromSliceError),

    #[error("no execution payload for this request")]
    ExecutionPayloadNotFound,

    #[error("builder not found for pubkey {pub_key:?}")]
    BuilderNotFound { pub_key: BlsPublicKeyBytes },
}

impl IntoResponse for AuctioneerError {
    fn into_response(self) -> Response {
        let code = match self {
            AuctioneerError::UnexpectedValueType |
            AuctioneerError::CryptoError(_) |
            AuctioneerError::FromUtf8Error(_) |
            AuctioneerError::ParseIntError(_) |
            AuctioneerError::FromHexError(_) |
            AuctioneerError::PastSlotAlreadyDelivered |
            AuctioneerError::AnotherPayloadAlreadyDeliveredForSlot |
            AuctioneerError::SszDeserializeError(_) |
            AuctioneerError::SliceConversionError(_) |
            AuctioneerError::ExecutionPayloadNotFound |
            AuctioneerError::BuilderNotFound { .. } => StatusCode::BAD_REQUEST,
        };

        (code, self.to_string()).into_response()
    }
}

#[derive(Clone)]
pub struct LocalCache {
    // TODO: this should be an ArcSwap
    pub inclusion_list: Arc<RwLock<Option<InclusionListWithKey>>>,
    builder_info_cache: Arc<DashMap<BlsPublicKeyBytes, BuilderInfo>>,
    /// Api key -> builder pubkey
    pub api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,
    primev_proposers: Arc<DashSet<BlsPublicKeyBytes>>,
    kill_switch: Arc<AtomicBool>,
    proposer_duties: Arc<RwLock<Vec<BuilderGetValidatorsResponseEntry>>>,
    merged_blocks: Arc<DashMap<B256, MergedBlock>>,
    pub validator_registration_cache:
        Arc<DashMap<BlsPublicKeyBytes, SignedValidatorRegistrationEntry>>,
    pub pending_validator_registrations: Arc<DashSet<BlsPublicKeyBytes>>,
    pub known_validators_cache: Arc<RwLock<FxHashSet<BlsPublicKeyBytes>>>,
    pub adjustments_enabled: Arc<AtomicBool>,
    pub adjustments_failsafe_trigger: Arc<AtomicBool>,
}

impl LocalCache {
    pub fn new() -> Self {
        let builder_info_cache =
            Arc::new(DashMap::with_capacity(ESTIMATED_BUILDER_INFOS_UPPER_BOUND));
        let api_key_cache = Arc::new(DashMap::with_capacity(ESTIMATED_BUILDER_INFOS_UPPER_BOUND));
        let primev_proposers = Arc::new(DashSet::with_capacity(MAX_PRIMEV_PROPOSERS));
        let kill_switch = Arc::new(AtomicBool::new(false));
        let proposer_duties = Arc::new(RwLock::new(Vec::with_capacity(1000)));
        let merged_blocks = Arc::new(DashMap::with_capacity(1000));
        let validator_registration_cache = Arc::new(DashMap::with_capacity(1_800_000));
        let pending_validator_registrations = Arc::new(DashSet::with_capacity(20_000));
        let known_validators_cache = Arc::new(RwLock::new(FxHashSet::with_capacity_and_hasher(
            1_200_000,
            Default::default(),
        )));
        let adjustments_enabled = Arc::new(AtomicBool::new(false));
        let adjustments_failsafe_trigger = Arc::new(AtomicBool::new(false));

        Self {
            inclusion_list: Default::default(),
            builder_info_cache,
            api_key_cache,
            primev_proposers,
            kill_switch,
            proposer_duties,
            merged_blocks,
            validator_registration_cache,
            pending_validator_registrations,
            known_validators_cache,
            adjustments_enabled,
            adjustments_failsafe_trigger,
        }
    }

    pub fn new_test() -> Self {
        Self::new()
    }
}

impl Default for LocalCache {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalCache {
    pub fn get_builder_info(&self, builder_pub_key: &BlsPublicKeyBytes) -> Option<BuilderInfo> {
        Some(self.builder_info_cache.get(builder_pub_key)?.clone())
    }

    pub fn contains_api_key(&self, api_key: &str) -> bool {
        self.api_key_cache.contains_key(api_key)
    }

    pub fn validate_api_key(&self, api_key: &str, pubkey: &BlsPublicKeyBytes) -> bool {
        self.api_key_cache.get(api_key).is_some_and(|p| p.value().contains(pubkey))
    }

    /// Returns whether builder was optimistic before the demotion
    pub fn demote_builder(&self, builder_pub_key: &BlsPublicKeyBytes) -> bool {
        let Some(mut builder_info) = self.builder_info_cache.get_mut(builder_pub_key) else {
            return false;
        };

        if !builder_info.is_optimistic {
            return false;
        }

        builder_info.is_optimistic = false;
        builder_info.is_optimistic_for_regional_filtering = false;

        true
    }

    pub fn update_builder_infos(&self, builder_infos: &[BuilderConfig], clear_api_cache: bool) {
        if clear_api_cache {
            self.api_key_cache.clear();
        }

        for builder_info in builder_infos {
            if let Some(api_key) = builder_info.builder_info.api_key.as_ref() {
                self.api_key_cache.entry(api_key.clone()).or_default().push(builder_info.pub_key);
            }

            self.builder_info_cache.insert(builder_info.pub_key, builder_info.builder_info.clone());
        }

        CACHE_SIZE.with_label_values(&["builder_info"]).set(self.builder_info_cache.len() as f64);
        CACHE_SIZE.with_label_values(&["api_keys"]).set(self.api_key_cache.len() as f64);
    }

    pub fn update_primev_proposers(&self, primev_proposers: &[BlsPublicKeyBytes]) {
        self.primev_proposers.clear();
        for proposer in primev_proposers {
            self.primev_proposers.insert(*proposer);
        }
    }

    pub fn is_primev_proposer(&self, proposer_pub_key: &BlsPublicKeyBytes) -> bool {
        self.primev_proposers.contains(proposer_pub_key)
    }

    pub fn kill_switch_enabled(&self) -> bool {
        self.kill_switch.load(Ordering::Relaxed)
    }

    pub fn enable_kill_switch(&self) {
        self.kill_switch.store(true, Ordering::Relaxed);
    }

    pub fn disable_kill_switch(&self) {
        self.kill_switch.store(false, Ordering::Relaxed);
    }

    pub fn update_current_inclusion_list(
        &self,
        inclusion_list: InclusionListWithMetadata,
        slot_coordinate: SlotCoordinate,
    ) {
        let new_list = InclusionListWithKey { key: slot_coordinate, inclusion_list };
        self.inclusion_list.write().replace(new_list);
    }

    pub fn update_proposer_duties(&self, duties: Vec<BuilderGetValidatorsResponseEntry>) {
        *self.proposer_duties.write() = duties;
    }

    pub fn get_proposer_duties(&self) -> Vec<BuilderGetValidatorsResponseEntry> {
        self.proposer_duties.read().clone()
    }

    pub fn save_merged_block(&self, merged_block: MergedBlock) {
        self.merged_blocks.insert(merged_block.block_hash(), merged_block);
        CACHE_SIZE.with_label_values(&["merged_blocks"]).set(self.merged_blocks.len() as f64);
    }

    pub fn get_merged_block(&self, block_hash: &B256) -> Option<MergedBlock> {
        self.merged_blocks.get(block_hash).map(|b| b.value().clone())
    }

    pub fn is_registration_update_required(
        &self,
        registration: &SignedValidatorRegistration,
    ) -> bool {
        if let Some(existing_entry) =
            self.validator_registration_cache.get(&registration.message.pubkey) &&
            existing_entry.registration_info.registration.message.timestamp >=
                registration
                    .message
                    .timestamp
                    .saturating_sub(VALIDATOR_REGISTRATION_UPDATE_INTERVAL) &&
            existing_entry.registration_info.registration.message.fee_recipient ==
                registration.message.fee_recipient &&
            existing_entry.registration_info.registration.message.gas_limit ==
                registration.message.gas_limit
        {
            // do registration once per hour, unless fee recipient / gas limit has changed

            return false;
        }
        true
    }

    /// Assume the entries are already validated
    pub fn save_validator_registrations(
        &self,
        entries: impl Iterator<Item = ValidatorRegistrationInfo>,
        user_agent: Option<String>,
    ) {
        for entry in entries {
            self.pending_validator_registrations.insert(entry.registration.message.pubkey);
            self.validator_registration_cache.insert(
                entry.registration.message.pubkey,
                SignedValidatorRegistrationEntry::new(entry.clone(), user_agent.clone()),
            );
        }
    }

    pub fn get_validator_registrations_for_pub_keys(
        &self,
        pub_keys: &[BlsPublicKeyBytes],
    ) -> Vec<SignedValidatorRegistrationEntry> {
        let mut registrations = Vec::with_capacity(pub_keys.len());
        for pub_key in pub_keys {
            if let Some(entry) = self.validator_registration_cache.get(pub_key) {
                registrations.push(entry.clone());
            }
        }
        registrations
    }

    pub fn get_merged_blocks(&self) -> Vec<MergedBlock> {
        self.merged_blocks.iter().map(|b| b.value().clone()).collect()
    }

    pub fn clear_merged_blocks(&self) {
        self.merged_blocks.clear();
        CACHE_SIZE.with_label_values(&["merged_blocks"]).set(0.0);
    }
}

#[cfg(test)]
mod tests {

    use alloy_primitives::U256;

    use super::*;
    use crate::BuilderConfig;

    #[tokio::test]
    pub async fn test_get_builder_info() {
        let cache = LocalCache::new();

        let builder_pub_key = BlsPublicKeyBytes::random();
        let unknown_builder_pub_key = BlsPublicKeyBytes::random();

        let builder_info = BuilderInfo {
            collateral: U256::from(12),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: false,
            builder_id: None,
            builder_ids: None,
            api_key: None,
        };

        // Test case 1: Builder exists
        let builder_info_doc =
            BuilderConfig { pub_key: builder_pub_key, builder_info: builder_info.clone() };
        cache.update_builder_infos(&[builder_info_doc], false);

        let get_result = cache.get_builder_info(&builder_pub_key);
        assert!(get_result.is_some(), "Failed to get builder info");
        assert_eq!(
            get_result.unwrap().collateral,
            builder_info.collateral,
            "Builder info mismatch"
        );

        // Test case 2: Builder doesn't exist
        let result = cache.get_builder_info(&unknown_builder_pub_key);
        assert!(result.is_none(), "Fetched builder info for unknown builder");
    }

    #[tokio::test]
    pub async fn test_kill_switch() {
        let cache = LocalCache::new();

        let result = cache.kill_switch_enabled();
        assert!(!result, "Kill switch should be disabled by default");

        cache.enable_kill_switch();

        let result = cache.kill_switch_enabled();
        assert!(result, "Kill switch should be enabled");

        cache.disable_kill_switch();

        let result = cache.kill_switch_enabled();
        assert!(!result, "Kill switch should be disabled");
    }
}
