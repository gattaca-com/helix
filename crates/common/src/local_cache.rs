use std::{
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use alloy_primitives::{B256, U256};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use dashmap::{DashMap, DashSet};
use helix_types::{BlsPublicKeyBytes, CryptoError, MergedBlock, SignedValidatorRegistration};
use libp2p::identity::PublicKey;
use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};
use uuid::Uuid;

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

#[derive(Debug, PartialEq, Eq)]
pub enum RegistrationUpdate {
    Required,
    Unchanged,
    IpMismatch,
}

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
    operator_builder_collateral: Arc<DashMap<BlsPublicKeyBytes, FxHashMap<PublicKey, u128>>>,
    /// Api key -> builder pubkey
    pub api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,
    primev_proposers: Arc<DashSet<BlsPublicKeyBytes>>,
    kill_switch: Arc<AtomicBool>,
    /// Production safety valve: whether `get_header` serves merged blocks to the proposer.
    /// Seeded at startup from `BlockMergingConfig::serve_merged_headers`, but can also be
    /// toggled live via the admin API, same as `kill_switch`.
    serve_merged_headers: Arc<AtomicBool>,
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
        let operator_builder_collateral =
            Arc::new(DashMap::with_capacity(ESTIMATED_BUILDER_INFOS_UPPER_BOUND));
        let api_key_cache = Arc::new(DashMap::with_capacity(ESTIMATED_BUILDER_INFOS_UPPER_BOUND));
        let primev_proposers = Arc::new(DashSet::with_capacity(MAX_PRIMEV_PROPOSERS));
        let kill_switch = Arc::new(AtomicBool::new(false));
        let serve_merged_headers = Arc::new(AtomicBool::new(false));
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
            operator_builder_collateral,
            api_key_cache,
            primev_proposers,
            kill_switch,
            serve_merged_headers,
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
        let mut info = self.builder_info_cache.get(builder_pub_key)?.clone();
        let operator_collateral = self.operator_collateral(builder_pub_key);
        info.collateral += operator_collateral;
        Some(info)
    }

    pub fn get_builder_info_local_collateral_only(
        &self,
        builder_pub_key: &BlsPublicKeyBytes,
    ) -> Option<BuilderInfo> {
        Some(self.builder_info_cache.get(builder_pub_key)?.clone())
    }

    pub fn all_builder_infos_local_collateral_only(
        &self,
    ) -> impl Iterator<Item = (BlsPublicKeyBytes, BuilderInfo)> {
        self.builder_info_cache.iter().map(|mref| (mref.key().clone(), mref.value().clone()))
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

        true
    }

    /// Returns whether builder was non-optimistic before the promotion
    pub fn promote_builder(&self, builder_pub_key: &BlsPublicKeyBytes) -> bool {
        let Some(mut builder_info) = self.builder_info_cache.get_mut(builder_pub_key) else {
            return false;
        };

        if builder_info.is_optimistic {
            return false;
        }

        builder_info.is_optimistic = true;

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

    pub fn merged_headers_enabled(&self) -> bool {
        self.serve_merged_headers.load(Ordering::Relaxed)
    }

    pub fn enable_merged_headers(&self) {
        self.serve_merged_headers.store(true, Ordering::Relaxed);
    }

    pub fn disable_merged_headers(&self) {
        self.serve_merged_headers.store(false, Ordering::Relaxed);
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

    pub fn set_merged_block_header_served(
        &self,
        block_hash: &B256,
        time_ns: u64,
        was_top_builder: bool,
    ) {
        if let Some(mut b) = self.merged_blocks.get_mut(block_hash) {
            b.trace.header_served_time_ns = Some(time_ns);
            b.trace.was_top_builder = Some(was_top_builder);
        }
    }

    pub fn set_merged_block_top_bid(&self, block_hash: &B256, top_bid: U256) {
        if let Some(mut b) = self.merged_blocks.get_mut(block_hash) {
            b.trace.top_bid = Some(top_bid);
        }
    }

    pub fn get_merged_block(&self, block_hash: &B256) -> Option<MergedBlock> {
        self.merged_blocks.get(block_hash).map(|b| b.value().clone())
    }

    pub fn registration_update(
        &self,
        registration: &SignedValidatorRegistration,
        api_key: Option<Uuid>,
        ip_addr: Option<IpAddr>,
    ) -> RegistrationUpdate {
        let Some(existing_entry) =
            self.validator_registration_cache.get(&registration.message.pubkey)
        else {
            return RegistrationUpdate::Required;
        };

        let existing = &existing_entry.registration_info.registration.message;
        let new = &registration.message;

        let fee_recipient_changed = existing.fee_recipient != new.fee_recipient;
        let gas_limit_changed = existing.gas_limit != new.gas_limit;

        let resigned =
            (fee_recipient_changed || gas_limit_changed || existing.timestamp != new.timestamp) &&
                new.timestamp >= existing.timestamp;

        // Registrations guard the api key update, so a payload anyone could have replayed needs the
        // ip of the last valid registration.
        if !resigned && existing_entry.ip_addr.is_some_and(|last| Some(last) != ip_addr) {
            return RegistrationUpdate::IpMismatch;
        }

        if existing.timestamp < new.timestamp.saturating_sub(VALIDATOR_REGISTRATION_UPDATE_INTERVAL) ||
            fee_recipient_changed ||
            gas_limit_changed ||
            existing_entry.registration_info.preferences.api_key != api_key
        {
            RegistrationUpdate::Required
        } else {
            RegistrationUpdate::Unchanged
        }
    }

    /// Assume the entries are already validated
    pub fn save_validator_registrations(
        &self,
        entries: impl Iterator<Item = ValidatorRegistrationInfo>,
        user_agent: Option<String>,
        ip_addr: Option<IpAddr>,
    ) {
        for entry in entries {
            self.pending_validator_registrations.insert(entry.registration.message.pubkey);
            self.validator_registration_cache.insert(
                entry.registration.message.pubkey,
                SignedValidatorRegistrationEntry::new(entry.clone(), user_agent.clone(), ip_addr),
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

    pub fn update_operator_collateral(
        &self,
        builder_pub_key: &BlsPublicKeyBytes,
        operator: &PublicKey,
        collateral: u128,
    ) {
        self.operator_builder_collateral
            .entry(*builder_pub_key)
            .and_modify(|operators| {
                operators.insert(operator.clone(), collateral);
            })
            .or_insert_with(|| {
                let mut operators = FxHashMap::default();
                operators.insert(operator.clone(), collateral);
                operators
            });
    }

    fn operator_collateral(&self, builder_pub_key: &BlsPublicKeyBytes) -> U256 {
        let mut operator_collateral = U256::ZERO;
        if let Some(operators) = self.operator_builder_collateral.get(builder_pub_key) {
            let collateral: u128 = operators.values().sum();
            operator_collateral = U256::from(collateral);
        }
        operator_collateral
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

    #[tokio::test]
    pub async fn test_serve_merged_headers() {
        let cache = LocalCache::new();

        let result = cache.merged_headers_enabled();
        assert!(result, "Merged headers should be served by default");

        cache.disable_merged_headers();

        let result = cache.merged_headers_enabled();
        assert!(!result, "Merged headers should not be served");

        cache.enable_merged_headers();

        let result = cache.merged_headers_enabled();
        assert!(result, "Merged headers should be served");
    }
}
