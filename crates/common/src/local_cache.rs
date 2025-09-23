use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use alloy_primitives::B256;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use dashmap::{DashMap, DashSet};
use helix_types::{BlsPublicKeyBytes, CryptoError, ForkName, PayloadAndBlobs};
use http::HeaderValue;
use parking_lot::RwLock;
use tokio::sync::broadcast;
use tracing::{error, info, instrument, warn};

use crate::{
    api::builder_api::{
        BuilderGetValidatorsResponseEntry, InclusionListWithKey, InclusionListWithMetadata,
        SlotCoordinate,
    },
    BuilderConfig, BuilderInfo, ProposerInfo,
};

type ExecutionPayloadKey = (u64, BlsPublicKeyBytes, B256);

const ESTIMATED_TRUSTED_PROPOSERS: usize = 200_000;
const ESTIMATED_BID_UPPER_BOUND: usize = 10_000;
const ESTIMATED_BUILDER_INFOS_UPPER_BOUND: usize = 1000;
const MAX_PRIMEV_PROPOSERS: usize = 64;

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
    inclusion_list: broadcast::Sender<InclusionListWithKey>,

    seen_block_hashes: Arc<DashSet<B256>>,
    last_delivered_slot: Arc<AtomicU64>,
    last_delivered_hash: Arc<RwLock<Option<B256>>>,
    builder_info_cache: Arc<DashMap<BlsPublicKeyBytes, BuilderInfo>>,
    /// Api key -> builder pubkey
    api_key_cache: Arc<DashMap<HeaderValue, Vec<BlsPublicKeyBytes>>>,
    trusted_proposers: Arc<DashMap<BlsPublicKeyBytes, ProposerInfo>>,
    execution_payload_cache: Arc<DashMap<ExecutionPayloadKey, PayloadAndBlobs>>,
    payload_address_cache: Arc<DashMap<B256, (BlsPublicKeyBytes, Vec<u8>)>>,
    primev_proposers: Arc<DashSet<BlsPublicKeyBytes>>,
    kill_switch: Arc<AtomicBool>,
    proposer_duties: Arc<RwLock<Vec<BuilderGetValidatorsResponseEntry>>>,
    headers_served: Arc<DashSet<B256>>,
}

#[allow(dead_code)]
impl LocalCache {
    pub fn new() -> Self {
        let (inclusion_list, _) = broadcast::channel(1);

        let seen_block_hashes = Arc::new(DashSet::with_capacity(ESTIMATED_BID_UPPER_BOUND));
        let builder_info_cache =
            Arc::new(DashMap::with_capacity(ESTIMATED_BUILDER_INFOS_UPPER_BOUND));
        let api_key_cache = Arc::new(DashMap::with_capacity(ESTIMATED_TRUSTED_PROPOSERS));
        let last_delivered_slot = Arc::new(AtomicU64::new(0));
        let last_delivered_hash = Arc::new(RwLock::new(None));
        let execution_payload_cache = Arc::new(DashMap::with_capacity(ESTIMATED_BID_UPPER_BOUND));
        let trusted_proposers = Arc::new(DashMap::with_capacity(ESTIMATED_TRUSTED_PROPOSERS));
        let payload_address_cache = Arc::new(DashMap::with_capacity(ESTIMATED_BID_UPPER_BOUND));
        let primev_proposers = Arc::new(DashSet::with_capacity(MAX_PRIMEV_PROPOSERS));
        let kill_switch = Arc::new(AtomicBool::new(false));
        let proposer_duties = Arc::new(RwLock::new(Vec::new()));
        let headers_served = Arc::new(DashSet::with_capacity(ESTIMATED_BID_UPPER_BOUND));

        Self {
            inclusion_list,
            seen_block_hashes,
            last_delivered_slot,
            last_delivered_hash,
            builder_info_cache,
            api_key_cache,
            trusted_proposers,
            execution_payload_cache,
            payload_address_cache,
            primev_proposers,
            kill_switch,
            proposer_duties,
            headers_served,
        }
    }

    pub fn new_test() -> Self {
        Self::new()
    }

    pub fn get_last_hash_delivered(&self) -> Option<B256> {
        *self.last_delivered_hash.read()
    }
}

impl LocalCache {
    #[instrument(skip_all)]
    pub fn get_last_slot_delivered(&self) -> Option<u64> {
        let last_slot_delivered = self.last_delivered_slot.load(Ordering::Relaxed);
        if last_slot_delivered == 0 {
            return None;
        }

        Some(last_slot_delivered)
    }

    #[instrument(skip_all)]
    pub fn save_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKeyBytes,
        block_hash: &B256,
        execution_payload: PayloadAndBlobs,
    ) {
        self.execution_payload_cache
            .insert((slot, *proposer_pub_key, *block_hash), execution_payload);
    }

    #[instrument(skip_all)]
    pub fn get_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKeyBytes,
        block_hash: &B256,
        _fork_name: ForkName,
    ) -> Option<PayloadAndBlobs> {
        self.execution_payload_cache.get(&(slot, *proposer_pub_key, *block_hash)).map(|p| {
            PayloadAndBlobs {
                execution_payload: p.execution_payload.clone(),
                blobs_bundle: p.blobs_bundle.clone(),
            }
        })
    }

    #[instrument(skip_all)]
    pub fn get_builder_info(
        &self,
        builder_pub_key: &BlsPublicKeyBytes,
    ) -> Result<BuilderInfo, AuctioneerError> {
        match self.builder_info_cache.get(builder_pub_key) {
            Some(cached) => Ok(cached.clone()),
            None => Err(AuctioneerError::BuilderNotFound { pub_key: *builder_pub_key }),
        }
    }

    #[instrument(skip_all)]
    pub fn contains_api_key(&self, api_key: &HeaderValue) -> bool {
        self.api_key_cache.contains_key(api_key)
    }

    #[instrument(skip_all)]
    pub fn validate_api_key(&self, api_key: &HeaderValue, pubkey: &BlsPublicKeyBytes) -> bool {
        self.api_key_cache.get(api_key).is_some_and(|p| p.value().contains(pubkey))
    }

    #[instrument(skip_all)]
    pub fn update_builder_infos(&self, builder_infos: &[BuilderConfig], clear_api_cache: bool) {
        if clear_api_cache {
            self.api_key_cache.clear();
        }

        for builder_info in builder_infos {
            if let Some(api_key) = builder_info.builder_info.api_key.as_ref() {
                self.api_key_cache
                    .entry(HeaderValue::from_str(api_key).unwrap())
                    .or_default()
                    .push(builder_info.pub_key);
            }

            self.builder_info_cache.insert(builder_info.pub_key, builder_info.builder_info.clone());
        }
    }

    #[instrument(skip_all)]
    pub fn seen_or_insert_block_hash(&self, block_hash: &B256) -> bool {
        !self.seen_block_hashes.insert(*block_hash)
    }

    #[instrument(skip_all)]
    pub fn update_trusted_proposers(&self, proposer_whitelist: Vec<ProposerInfo>) {
        for proposer in &proposer_whitelist {
            self.trusted_proposers.insert(proposer.pubkey, proposer.clone());
        }
    }

    #[instrument(skip_all)]
    pub fn is_trusted_proposer(&self, proposer_pub_key: &BlsPublicKeyBytes) -> bool {
        self.trusted_proposers.contains_key(proposer_pub_key)
    }

    #[instrument(skip_all)]
    pub fn update_primev_proposers(&self, primev_proposers: &[BlsPublicKeyBytes]) {
        self.primev_proposers.clear();
        for proposer in primev_proposers {
            self.primev_proposers.insert(*proposer);
        }
    }

    #[instrument(skip_all)]
    pub fn is_primev_proposer(&self, proposer_pub_key: &BlsPublicKeyBytes) -> bool {
        self.primev_proposers.contains(proposer_pub_key)
    }

    #[instrument(skip_all)]
    pub fn get_payload_url(&self, block_hash: &B256) -> Option<(BlsPublicKeyBytes, Vec<u8>)> {
        self.payload_address_cache.get(block_hash).map(|r| r.value().clone())
    }

    #[instrument(skip_all)]
    pub fn save_payload_address(
        &self,
        block_hash: &B256,
        builder_pub_key: &BlsPublicKeyBytes,
        payload_socket_address: Vec<u8>,
    ) {
        self.payload_address_cache.insert(*block_hash, (*builder_pub_key, payload_socket_address));
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

    // TODO:
    #[instrument(skip_all)]
    pub fn update_current_inclusion_list(
        &self,
        inclusion_list: InclusionListWithMetadata,
        slot_coordinate: SlotCoordinate,
    ) {
        let list_with_key = InclusionListWithKey { key: slot_coordinate, inclusion_list };
        if let Err(err) = self.inclusion_list.send(list_with_key) {
            error!(%err, "Failed to send inclusion list update");
        }
    }

    #[instrument(skip_all)]
    pub fn update_proposer_duties(&self, duties: Vec<BuilderGetValidatorsResponseEntry>) {
        *self.proposer_duties.write() = duties;
    }

    #[instrument(skip_all)]
    pub fn get_proposer_duties(&self) -> Vec<BuilderGetValidatorsResponseEntry> {
        self.proposer_duties.read().clone()
    }

    #[instrument(skip_all)]
    pub fn process_slot(&self, head_slot: u64) {
        info!(head_slot, "Processing new slot in local cache, clearing old data");

        self.seen_block_hashes.clear();
        self.execution_payload_cache.clear();
        self.payload_address_cache.clear();
        self.headers_served.clear();
    }

    #[instrument(skip_all)]
    pub fn mark_header_served(&self, block_hash: &B256) {
        self.headers_served.insert(*block_hash);
    }

    #[instrument(skip_all)]
    pub fn has_header_been_served(&self, block_hash: &B256) -> bool {
        self.headers_served.contains(block_hash)
    }
}

#[cfg(test)]
mod tests {

    use alloy_primitives::U256;
    use helix_types::{
        get_fixed_pubkey, get_fixed_pubkey_bytes, BlobsBundle, BlsPublicKey, ExecutionPayload,
        ForkName, PayloadAndBlobsRef, TestRandomSeed,
    };

    use super::*;
    use crate::BuilderConfig;

    #[tokio::test]
    pub async fn test_get_and_save_execution_payload() {
        let cache = LocalCache::new();

        let slot = 42;
        let proposer_pub_key = BlsPublicKey::test_random();
        let block_hash = B256::test_random();

        let payload = ExecutionPayload { gas_limit: 999, ..ExecutionPayload::test_random() };
        let blobs_bundle = BlobsBundle::default();
        let versioned_execution_payload =
            PayloadAndBlobsRef { execution_payload: &payload, blobs_bundle: &blobs_bundle };

        // Save the execution payload
        cache.save_execution_payload(
            slot,
            &proposer_pub_key.serialize().into(),
            &block_hash,
            versioned_execution_payload.to_owned(),
        );

        // Test: Get the execution payload
        let get_result: Option<PayloadAndBlobs> = cache.get_execution_payload(
            slot,
            &proposer_pub_key.serialize().into(),
            &block_hash,
            ForkName::Electra,
        );
        assert!(get_result.as_ref().is_some(), "Execution payload is None");

        let fetched_execution_payload = get_result.unwrap();
        assert_eq!(
            fetched_execution_payload.execution_payload.gas_limit, 999,
            "Execution payload mismatch"
        );
    }

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
        assert!(get_result.is_ok(), "Failed to get builder info");
        assert_eq!(
            get_result.unwrap().collateral,
            builder_info.collateral,
            "Builder info mismatch"
        );

        // Test case 2: Builder doesn't exist
        let result = cache.get_builder_info(&unknown_builder_pub_key);
        assert!(result.is_err(), "Fetched builder info for unknown builder");
        assert!(
            matches!(result.unwrap_err(), AuctioneerError::BuilderNotFound { .. }),
            "Incorrect get builder info error"
        );
    }

    #[tokio::test]
    pub async fn test_get_trusted_proposers_and_update_trusted_proposers() {
        let cache = LocalCache::new();

        let is_trusted = cache.is_trusted_proposer(&BlsPublicKey::test_random().serialize().into());
        assert!(!is_trusted, "Failed to check trusted proposer");

        cache.update_trusted_proposers(vec![
            ProposerInfo { name: "test".to_string(), pubkey: get_fixed_pubkey_bytes(0) },
            ProposerInfo { name: "test2".to_string(), pubkey: get_fixed_pubkey_bytes(1) },
        ]);

        let is_trusted = cache.is_trusted_proposer(&get_fixed_pubkey_bytes(0));
        assert!(is_trusted, "Failed to check trusted proposer");

        let is_trusted = cache.is_trusted_proposer(&get_fixed_pubkey_bytes(1));
        assert!(is_trusted, "Failed to check trusted proposer");

        let is_trusted = cache.is_trusted_proposer(&get_fixed_pubkey_bytes(2));
        assert!(!is_trusted, "Failed to check trusted proposer");

        cache.update_trusted_proposers(vec![ProposerInfo {
            name: "test2".to_string(),
            pubkey: get_fixed_pubkey_bytes(3),
        }]);

        let is_trusted = cache.is_trusted_proposer(&BlsPublicKey::test_random().serialize().into());
        assert!(!is_trusted, "Failed to check trusted proposer");

        let is_trusted = cache.is_trusted_proposer(&get_fixed_pubkey_bytes(3));
        assert!(is_trusted, "Failed to check trusted proposer");
    }

    #[tokio::test]
    pub async fn test_seen_or_insert_block_hash() {
        let cache = LocalCache::new();

        let _slot = 42;
        let block_hash = B256::random();
        let _pubkey = get_fixed_pubkey(0);

        // Test: Check if block hash has been seen before (should be false initially)
        let seen_result = cache.seen_or_insert_block_hash(&block_hash);
        assert!(!seen_result, "Block hash was incorrectly seen before");

        // Test: Insert the block hash and check again (should be true after insert)
        let seen_result_again = cache.seen_or_insert_block_hash(&block_hash);
        assert!(seen_result_again, "Block hash was not seen after insert");

        // Test: Add a different new block hash (should be false initially)
        let block_hash_2 = B256::random();
        let seen_result = cache.seen_or_insert_block_hash(&block_hash_2);
        assert!(!seen_result, "Block hash was incorrectly seen before");

        // Test: Insert the original block hash again, ensure it wasn't overwritten (should be true
        // after insert)
        let seen_result_again = cache.seen_or_insert_block_hash(&block_hash);
        assert!(seen_result_again, "Block hash was not seen after insert");
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
