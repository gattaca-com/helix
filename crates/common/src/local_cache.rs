use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use alloy_primitives::{B256, U256};
use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use dashmap::{DashMap, DashSet};
use helix_types::{BlsPublicKeyBytes, CryptoError, MergedBlock, SignedValidatorRegistration};
use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};
use uuid::Uuid;

use crate::{
    BuilderConfig, BuilderInfo, PromotionMode, SignedValidatorRegistrationEntry,
    api::{
        builder_api::{
            BuilderGetValidatorsResponseEntry, InclusionListWithKey, InclusionListWithMetadata,
            SlotCoordinate,
        },
        proposer_api::ValidatorRegistrationInfo,
    },
    api_provider::ApiProvider,
    metrics::CACHE_SIZE,
};

#[derive(Debug, PartialEq, Eq)]
pub enum RegistrationUpdate {
    Required,
    Unchanged,
    Rejected,
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

/// Per-pubkey half of the derived view. The pool index keys [`PoolView`].
#[derive(Clone, Copy)]
pub struct PoolBinding {
    pub pool: u32,
    /// This key has a retained demotion report.
    pub reported: bool,
}

/// Pool half of the derived view. Held once per pool, so members cannot disagree about the
/// budget they share.
#[derive(Clone)]
pub struct PoolView {
    pub collateral_id: Vec<u8>,
    /// Gross backing net of reservations.
    pub available: U256,
    pub promoted: bool,
}

/// What the submission path resolves for one pubkey.
pub struct PoolStatus {
    pub collateral_id: Vec<u8>,
    pub available: U256,
    pub is_optimistic: bool,
}

#[derive(Clone)]
pub struct LocalCache {
    // TODO: this should be an ArcSwap
    pub inclusion_list: Arc<RwLock<Option<InclusionListWithKey>>>,
    builder_info_cache: Arc<DashMap<BlsPublicKeyBytes, BuilderInfo>>,
    /// Derived view, written by the operator task and read on the submission path once
    /// `promotion_mode` applies. Split so the shared budget is stored once per pool rather than
    /// replicated per member. An absent binding or view means the pool has not been recovered.
    pool_binding: Arc<DashMap<BlsPublicKeyBytes, PoolBinding>>,
    pool_view: Arc<DashMap<u32, PoolView>>,
    promotion_mode: Arc<RwLock<PromotionMode>>,
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
        let pool_binding = Arc::new(DashMap::with_capacity(ESTIMATED_BUILDER_INFOS_UPPER_BOUND));
        let pool_view = Arc::new(DashMap::default());
        let api_key_cache = Arc::new(DashMap::with_capacity(ESTIMATED_BUILDER_INFOS_UPPER_BOUND));
        let primev_proposers = Arc::new(DashSet::with_capacity(MAX_PRIMEV_PROPOSERS));
        let kill_switch = Arc::new(AtomicBool::new(false));
        let serve_merged_headers = Arc::new(AtomicBool::new(true));
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
            pool_binding,
            pool_view,
            promotion_mode: Arc::new(RwLock::new(PromotionMode::default())),
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
        if !self.promotion_mode().applies() {
            // Locally configured collateral only. Contributions from other operator groups are
            // used from `Share`, where membership is validated and reservations are applied.
            return Some(info);
        }

        // Pool state is authoritative: collateral is net of reservations and status derives from
        // the pool promotion and this key's retained reports. A pubkey with no binding, or whose
        // pool has no view, belongs to a pool that has not been recovered and stays ineligible.
        match self
            .pool_binding
            .get(builder_pub_key)
            .and_then(|binding| Some((*binding, self.pool_view.get(&binding.pool)?)))
        {
            Some((binding, view)) => {
                info.collateral = view.available;
                info.is_optimistic = view.promoted && !binding.reported;
            }
            None => {
                info.collateral = U256::ZERO;
                info.is_optimistic = false;
            }
        }
        Some(info)
    }

    pub fn promotion_mode(&self) -> PromotionMode {
        *self.promotion_mode.read()
    }

    pub fn set_promotion_mode(&self, mode: PromotionMode) {
        *self.promotion_mode.write() = mode;
    }

    /// Replaces the derived view. Written by the operator task when pool state changes.
    ///
    /// Views first: a binding written before its view would read as unrecovered. Nothing is
    /// removed, because pools are never dropped and membership is append-only.
    pub fn update_pool_state(
        &self,
        views: impl Iterator<Item = (u32, Vec<u8>, U256, bool)>,
        bindings: impl Iterator<Item = (BlsPublicKeyBytes, u32, bool)>,
    ) {
        for (pool, collateral_id, available, promoted) in views {
            self.pool_view.insert(pool, PoolView { collateral_id, available, promoted });
        }
        for (pubkey, pool, reported) in bindings {
            self.pool_binding.insert(pubkey, PoolBinding { pool, reported });
        }
    }

    pub fn get_builder_info_local_collateral_only(
        &self,
        builder_pub_key: &BlsPublicKeyBytes,
    ) -> Option<BuilderInfo> {
        Some(self.builder_info_cache.get(builder_pub_key)?.clone())
    }

    /// Pool membership and gross backing, keyed by builder id (the pool id).
    ///
    /// A builder id group shares one collateral value, so take the maximum rather than the first
    /// row: `DashMap` iteration order is unspecified, and this result is published to other
    /// operators. Zero-collateral pools are included; omission is not removal.
    pub fn all_builder_local_collateral(
        &self,
    ) -> FxHashMap<String, (Vec<BlsPublicKeyBytes>, U256)> {
        let mut builders = FxHashMap::<String, (Vec<BlsPublicKeyBytes>, U256)>::default();
        for mref in self.builder_info_cache.iter() {
            let Some(id) = mref.value().builder_id.as_ref() else {
                continue;
            };
            let collateral = mref.value().collateral;
            builders
                .entry(id.to_owned())
                .and_modify(|(keys, wei)| {
                    keys.push(*mref.key());
                    *wei = (*wei).max(collateral);
                })
                .or_insert((vec![*mref.key()], collateral));
        }
        builders
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

    /// Resolved derived view for a pubkey. `None` until pool state has been recovered.
    pub fn pool_status(&self, builder_pub_key: &BlsPublicKeyBytes) -> Option<PoolStatus> {
        let binding = *self.pool_binding.get(builder_pub_key)?;
        let view = self.pool_view.get(&binding.pool)?;
        Some(PoolStatus {
            collateral_id: view.collateral_id.clone(),
            available: view.available,
            is_optimistic: view.promoted && !binding.reported,
        })
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
        headers: &HeaderMap,
        api_provider: &impl ApiProvider,
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

        if !api_provider.admit_registration(resigned, existing_entry.value(), headers) {
            return RegistrationUpdate::Rejected;
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

    /// A builder id group is one pool. The published value must not depend on `DashMap`
    /// iteration order, and a zero-backed pool must still be published: omission is not removal.
    #[test]
    fn all_builder_local_collateral_is_per_pool_and_order_independent() {
        let cache = LocalCache::new();
        let info = |collateral, builder_id: Option<&str>| BuilderInfo {
            collateral,
            is_optimistic: true,
            is_optimistic_for_regional_filtering: false,
            builder_id: builder_id.map(str::to_owned),
            builder_ids: None,
            api_key: None,
        };

        let [paired, sibling, zero, unpooled] = [0; 4].map(|_| BlsPublicKeyBytes::random());
        cache.update_builder_infos(
            &[
                BuilderConfig { pub_key: paired, builder_info: info(U256::from(10), Some("A")) },
                // Written out of step with its group, e.g. by the admin collateral endpoint.
                BuilderConfig { pub_key: sibling, builder_info: info(U256::ZERO, Some("A")) },
                BuilderConfig { pub_key: zero, builder_info: info(U256::ZERO, Some("B")) },
                BuilderConfig { pub_key: unpooled, builder_info: info(U256::from(5), None) },
            ],
            false,
        );

        let pools = cache.all_builder_local_collateral();
        assert_eq!(pools.len(), 2, "a row with no builder id belongs to no pool");

        let (members, collateral) = &pools["A"];
        assert_eq!(*collateral, U256::from(10));
        assert_eq!(members.len(), 2);

        assert_eq!(pools["B"].1, U256::ZERO, "a zero-backed pool is still published");
    }

    /// The mode decides who owns admission. Below `Follow`, `builder_info` plus summed operator
    /// collateral. From `Follow`, pool state: collateral net of reservations, derived status, and
    /// ineligible while a pool has no recovered state.
    #[test]
    fn promotion_mode_gates_whether_pool_state_decides_admission() {
        let cache = LocalCache::new();
        let pubkey = BlsPublicKeyBytes::random();
        cache.update_builder_infos(
            &[BuilderConfig {
                pub_key: pubkey,
                builder_info: BuilderInfo {
                    collateral: U256::from(10),
                    is_optimistic: true,
                    is_optimistic_for_regional_filtering: false,
                    builder_id: Some("A".to_owned()),
                    builder_ids: None,
                    api_key: None,
                },
            }],
            false,
        );
        let observed = cache.get_builder_info(&pubkey).unwrap();
        assert_eq!(observed.collateral, U256::from(10), "locally configured collateral only");
        assert!(observed.is_optimistic);

        // Pool state has not been recovered yet: the pool is ineligible.
        cache.set_promotion_mode(PromotionMode::Follow);
        let ungated = cache.get_builder_info(&pubkey).unwrap();
        assert_eq!(ungated.collateral, U256::ZERO);
        assert!(!ungated.is_optimistic);

        cache.update_pool_state(
            [(0, b"A".to_vec(), U256::from(7), true)].into_iter(),
            [(pubkey, 0, false)].into_iter(),
        );
        let followed = cache.get_builder_info(&pubkey).unwrap();
        assert_eq!(followed.collateral, U256::from(7), "available, net of reservations");
        assert!(followed.is_optimistic);

        // A retained report demotes the key alone; the pool budget is untouched.
        cache.update_pool_state(std::iter::empty(), [(pubkey, 0, true)].into_iter());
        let reported = cache.get_builder_info(&pubkey).unwrap();
        assert_eq!(reported.collateral, U256::from(7));
        assert!(!reported.is_optimistic);
    }

    /// A pool-wide promotion reaches admission through pool state, not through the per-pubkey
    /// flag. The flag stays false for a member that was never individually promoted.
    #[test]
    fn pool_state_outranks_the_per_pubkey_flag() {
        let [named, sibling] = [0; 2].map(|_| BlsPublicKeyBytes::random());
        let cache = LocalCache::new();
        let pessimistic = BuilderInfo {
            collateral: U256::from(10),
            is_optimistic: false,
            is_optimistic_for_regional_filtering: false,
            builder_id: Some("A".to_owned()),
            builder_ids: None,
            api_key: None,
        };
        cache.update_builder_infos(
            &[BuilderConfig { pub_key: named, builder_info: pessimistic.clone() }, BuilderConfig {
                pub_key: sibling,
                builder_info: pessimistic,
            }],
            false,
        );

        // Only the named pubkey is flagged, in every mode.
        assert!(cache.promote_builder(&named));
        assert!(!cache.get_builder_info_local_collateral_only(&sibling).unwrap().is_optimistic);

        // Pool state promotes the pool, so both are optimistic for admission from `Follow`.
        // One budget record covers both members.
        cache.set_promotion_mode(PromotionMode::Follow);
        cache.update_pool_state(
            [(0, b"A".to_vec(), U256::from(7), true)].into_iter(),
            [(named, 0, false), (sibling, 0, false)].into_iter(),
        );
        assert!(cache.get_builder_info(&sibling).unwrap().is_optimistic);
        assert_eq!(cache.get_builder_info(&sibling).unwrap().collateral, U256::from(7));
        assert!(
            !cache.get_builder_info_local_collateral_only(&sibling).unwrap().is_optimistic,
            "the stored flag is untouched"
        );
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
