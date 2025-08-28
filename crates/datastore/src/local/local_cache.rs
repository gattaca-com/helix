use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use alloy_primitives::B256;
use async_trait::async_trait;
use dashmap::{DashMap, DashSet};
use helix_common::{
    api::builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
    bid_sorter::BidSorterMessage,
    BuilderInfo, ProposerInfo,
};
use helix_database::types::BuilderInfoDocument;
use helix_types::{
    maybe_upgrade_execution_payload, BidTrace, BlsPublicKey, ForkName, PayloadAndBlobs,
    PayloadAndBlobsRef,
};
use moka::sync::Cache;
use parking_lot::RwLock;
use tokio::sync::broadcast;
use tracing::{error, instrument, warn};

use crate::{
    error::AuctioneerError,
    local::utils::{get_cache_bid_trace_key, get_execution_payload_key},
    types::keys::CURRENT_INCLUSION_LIST_KEY,
    Auctioneer,
};

const BID_CACHE_EXPIRY_S: u64 = 45;
const INCLUSION_LIST_EXPIRY_S: u64 = 45;
const PAYLOAD_ADDRESS_EXPIRY_S: u64 = 24;

#[derive(Clone)]
pub struct LocalCache {
    inclusion_list: broadcast::Sender<InclusionListWithKey>,

    seen_block_hashes: Cache<B256, ()>,
    last_delivered_slot: Arc<AtomicU64>,
    last_delivered_hash: Arc<RwLock<Option<B256>>>,
    // TODO: remove dead code. Currently if it's removed the compiler complains
    _builder_latest_payload_received_at: Cache<String, Arc<DashMap<String, u64>>>,
    builder_info_cache: DashMap<BlsPublicKey, BuilderInfo>,
    trusted_proposers: DashMap<BlsPublicKey, ProposerInfo>,
    execution_payload_cache: Cache<String, PayloadAndBlobs>,
    payload_address_cache: Cache<B256, (BlsPublicKey, Vec<u8>)>,
    bid_trace_cache: Cache<String, BidTrace>,
    primev_proposers: DashSet<BlsPublicKey>,
    kill_switch: Arc<AtomicBool>,
    inclusion_list_cache: Cache<String, InclusionListWithKey>,
    proposer_duties: Arc<RwLock<Vec<BuilderGetValidatorsResponseEntry>>>,

    sorter_tx: crossbeam_channel::Sender<BidSorterMessage>,
}

#[allow(dead_code)]
impl LocalCache {
    pub async fn new(
        builder_infos: Vec<BuilderInfoDocument>,
        sorter_tx: crossbeam_channel::Sender<BidSorterMessage>,
    ) -> Self {
        let (inclusion_list, mut il_recv) = broadcast::channel(1);

        // ensure at least one subscriber is running
        tokio::spawn(async move { while let Ok(_message) = il_recv.recv().await {} });

        let seen_block_hashes =
            Cache::builder().time_to_idle(Duration::from_secs(45)).max_capacity(100_000).build();
        let _builder_latest_payload_received_at =
            Cache::builder().time_to_idle(Duration::from_secs(45)).max_capacity(100_000).build();
        let builder_info_cache = DashMap::new();
        let last_delivered_slot = Arc::new(AtomicU64::new(0));
        let last_delivered_hash = Arc::new(RwLock::new(None));
        let execution_payload_cache =
            Cache::builder().time_to_idle(Duration::from_secs(45)).max_capacity(100_000).build();

        let trusted_proposers = DashMap::new();

        let payload_address_cache = Cache::builder()
            .time_to_idle(Duration::from_secs(PAYLOAD_ADDRESS_EXPIRY_S))
            .max_capacity(100_000)
            .build();

        let bid_trace_cache = Cache::builder()
            .time_to_idle(Duration::from_secs(BID_CACHE_EXPIRY_S))
            .max_capacity(100_000)
            .build();

        let primev_proposers = DashSet::new();
        let kill_switch = Arc::new(AtomicBool::new(false));

        let inclusion_list_cache = Cache::builder()
            .time_to_idle(Duration::from_secs(INCLUSION_LIST_EXPIRY_S))
            .max_capacity(10)
            .build();

        let proposer_duties = Arc::new(RwLock::new(Vec::new()));

        let cache = Self {
            inclusion_list,
            seen_block_hashes,
            _builder_latest_payload_received_at,
            last_delivered_slot,
            last_delivered_hash,
            builder_info_cache,
            trusted_proposers,
            execution_payload_cache,
            payload_address_cache,
            bid_trace_cache,
            primev_proposers,
            kill_switch,
            inclusion_list_cache,
            proposer_duties,
            sorter_tx,
        };

        // Load in builder info
        cache.update_builder_infos(&builder_infos);

        cache
    }

    fn get_last_hash_delivered(&self) -> Option<B256> {
        let maybe = {
            let guard = self.last_delivered_hash.read();
            *guard
        };

        maybe
    }
}

#[async_trait]
impl Auctioneer for LocalCache {
    #[instrument(skip_all)]
    fn get_last_slot_delivered(&self) -> Option<u64> {
        let last_slot_delivered = self.last_delivered_slot.load(Ordering::Relaxed);
        if last_slot_delivered == 0 {
            return None;
        }

        Some(last_slot_delivered)
    }

    #[instrument(skip_all)]
    fn check_and_set_last_slot_and_hash_delivered(
        &self,
        slot: u64,
        hash: &B256,
    ) -> Result<(), AuctioneerError> {
        let last_slot_delivered_res = self.get_last_slot_delivered();

        if let Some(last_slot_delivered) = last_slot_delivered_res {
            if slot < last_slot_delivered {
                return Err(AuctioneerError::PastSlotAlreadyDelivered);
            }

            if slot == last_slot_delivered {
                let last_hash_delivered_res = self.get_last_hash_delivered();

                match last_hash_delivered_res {
                    Some(last_hash_delivered) => {
                        if *hash != last_hash_delivered {
                            return Err(AuctioneerError::AnotherPayloadAlreadyDeliveredForSlot);
                        }
                    }
                    None => return Err(AuctioneerError::UnexpectedValueType),
                }

                return Ok(());
            }
        }

        self.last_delivered_slot.store(slot, Ordering::Relaxed);
        self.last_delivered_hash.write().replace(*hash);

        Ok(())
    }

    #[instrument(skip_all)]
    fn save_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &B256,
        execution_payload: PayloadAndBlobsRef,
    ) {
        let key = get_execution_payload_key(slot, proposer_pub_key, block_hash);
        self.execution_payload_cache.insert(key, execution_payload.to_owned());
    }

    #[instrument(skip_all)]
    fn get_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &B256,
        fork_name: ForkName,
    ) -> Option<PayloadAndBlobs> {
        let key = get_execution_payload_key(slot, proposer_pub_key, block_hash);
        self.execution_payload_cache.get(&key).map(|p| PayloadAndBlobs {
            execution_payload: maybe_upgrade_execution_payload(p.execution_payload, fork_name),
            blobs_bundle: p.blobs_bundle,
        })
    }

    #[instrument(skip_all)]
    fn get_bid_trace(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &B256,
    ) -> Option<BidTrace> {
        let key = get_cache_bid_trace_key(slot, proposer_pub_key, block_hash);
        self.bid_trace_cache.get(&key)
    }

    #[instrument(skip_all)]
    fn save_bid_trace(&self, bid_trace: &BidTrace) {
        let key = get_cache_bid_trace_key(
            bid_trace.slot,
            &bid_trace.proposer_pubkey,
            &bid_trace.block_hash,
        );
        self.bid_trace_cache.insert(key, bid_trace.to_owned());
    }

    #[instrument(skip_all)]
    fn get_builder_info(
        &self,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<BuilderInfo, AuctioneerError> {
        match self.builder_info_cache.get(builder_pub_key) {
            Some(cached) => Ok(cached.clone()),
            None => Err(AuctioneerError::BuilderNotFound { pub_key: builder_pub_key.clone() }),
        }
    }

    #[instrument(skip_all)]
    fn demote_builder(&self, builder_pub_key: &BlsPublicKey) -> Result<(), AuctioneerError> {
        let _ = self.sorter_tx.try_send(BidSorterMessage::Demotion(builder_pub_key.clone()));

        let mut builder_info = self.get_builder_info(builder_pub_key)?;
        if !builder_info.is_optimistic {
            return Ok(());
        }
        builder_info.is_optimistic = false;
        builder_info.is_optimistic_for_regional_filtering = false;

        self.builder_info_cache.insert(builder_pub_key.clone(), builder_info.clone());
        Ok(())
    }

    #[instrument(skip_all)]
    fn update_builder_infos(&self, builder_infos: &[BuilderInfoDocument]) {
        for builder_info in builder_infos {
            self.builder_info_cache
                .insert(builder_info.pub_key.clone(), builder_info.builder_info.clone());
        }
    }

    #[instrument(skip_all)]
    fn seen_or_insert_block_hash(&self, block_hash: &B256) -> bool {
        if self.seen_block_hashes.contains_key(block_hash) {
            return true;
        }

        self.seen_block_hashes.insert(*block_hash, ());
        false
    }

    #[instrument(skip_all)]
    fn update_trusted_proposers(&self, proposer_whitelist: Vec<ProposerInfo>) {
        for proposer in &proposer_whitelist {
            self.trusted_proposers.insert(proposer.pubkey.clone(), proposer.clone());
        }
    }

    #[instrument(skip_all)]
    fn is_trusted_proposer(&self, proposer_pub_key: &BlsPublicKey) -> bool {
        self.trusted_proposers.contains_key(proposer_pub_key)
    }

    #[instrument(skip_all)]
    fn update_primev_proposers(&self, primev_proposers: &[BlsPublicKey]) {
        self.primev_proposers.clear();
        for proposer in primev_proposers {
            self.primev_proposers.insert(proposer.clone());
        }
    }

    #[instrument(skip_all)]
    fn is_primev_proposer(&self, proposer_pub_key: &BlsPublicKey) -> bool {
        self.primev_proposers.contains(proposer_pub_key)
    }

    #[instrument(skip_all)]
    fn get_payload_url(&self, block_hash: &B256) -> Option<(BlsPublicKey, Vec<u8>)> {
        self.payload_address_cache.get(block_hash)
    }

    #[instrument(skip_all)]
    fn save_payload_address(
        &self,
        block_hash: &B256,
        builder_pub_key: &BlsPublicKey,
        payload_socket_address: Vec<u8>,
    ) {
        self.payload_address_cache
            .insert(*block_hash, (builder_pub_key.clone(), payload_socket_address));
    }

    fn kill_switch_enabled(&self) -> bool {
        self.kill_switch.load(Ordering::Relaxed)
    }

    //TODO: Implement kill switch functionality. Need to add an internal api to call this.
    fn enable_kill_switch(&self) {
        self.kill_switch.store(true, Ordering::Relaxed);
    }

    //TODO: Implement kill switch functionality. Need to add an internal api to call this.
    fn disable_kill_switch(&self) {
        self.kill_switch.store(false, Ordering::Relaxed);
    }

    #[instrument(skip_all)]
    fn update_current_inclusion_list(
        &self,
        inclusion_list: InclusionListWithMetadata,
        slot_coordinate: String,
    ) -> Result<(), AuctioneerError> {
        let new_inclusion_list_key = format!("{CURRENT_INCLUSION_LIST_KEY}_{slot_coordinate}");
        let list_with_key =
            InclusionListWithKey { key: new_inclusion_list_key.clone(), inclusion_list };
        self.inclusion_list_cache.insert(new_inclusion_list_key, list_with_key.clone());
        if let Err(err) = self.inclusion_list.send(list_with_key) {
            error!(%err, "Failed to send inclusion list update");
        }

        Ok(())
    }

    #[instrument(skip_all)]
    fn get_inclusion_list(&self) -> broadcast::Receiver<InclusionListWithKey> {
        self.inclusion_list.subscribe()
    }

    #[instrument(skip_all)]
    fn update_proposer_duties(&self, duties: Vec<BuilderGetValidatorsResponseEntry>) {
        *self.proposer_duties.write() = duties;
    }

    #[instrument(skip_all)]
    fn get_proposer_duties(&self) -> Vec<BuilderGetValidatorsResponseEntry> {
        self.proposer_duties.read().clone()
    }
}

#[derive(Clone, Debug)]
pub struct InclusionListWithKey {
    pub key: String,
    pub inclusion_list: InclusionListWithMetadata,
}

impl<'a> From<&'a InclusionListWithKey> for (&'a InclusionListWithMetadata, &'a str) {
    fn from(value: &'a InclusionListWithKey) -> Self {
        (&value.inclusion_list, &value.key)
    }
}

#[cfg(test)]
mod tests {

    use alloy_primitives::U256;
    use helix_common::BuilderConfig;
    use helix_types::{
        get_fixed_pubkey, BlobsBundle, ExecutionPayloadElectra, ExecutionPayloadRef, ForkName,
        TestRandomSeed,
    };

    use super::*;

    /// #######################################################################
    /// ########################### Auctioneer tests ##########################
    /// #######################################################################

    #[tokio::test]
    async fn test_get_and_check_last_slot_and_hash_delivered() {
        let cache = LocalCache::new(Vec::new(), crossbeam_channel::bounded(1).0).await;

        let slot = 42;
        let block_hash = B256::try_from([4u8; 32].as_ref()).unwrap();

        // Test: Save the last slot and hash delivered
        let set_result = cache.check_and_set_last_slot_and_hash_delivered(slot, &block_hash);
        assert!(set_result.is_ok(), "Saving last slot and hash delivered failed");

        // Test: Get the last slot delivered
        let get_result: Option<u64> = cache.get_last_slot_delivered();
        assert_eq!(get_result.unwrap(), slot, "Slot value mismatch");
    }

    #[tokio::test]
    async fn test_set_past_slot() {
        let cache = LocalCache::new(Vec::new(), crossbeam_channel::bounded(1).0).await;

        let slot = 42;
        let block_hash = B256::try_from([4u8; 32].as_ref()).unwrap();

        // Set a future slot
        assert!(cache.check_and_set_last_slot_and_hash_delivered(slot + 1, &block_hash).is_ok());

        // Test: Try to set a past slot
        let set_result = cache.check_and_set_last_slot_and_hash_delivered(slot, &block_hash);
        assert!(matches!(set_result, Err(AuctioneerError::PastSlotAlreadyDelivered)));
    }

    #[tokio::test]
    async fn test_set_same_slot_different_hash() {
        let cache = LocalCache::new(Vec::new(), crossbeam_channel::bounded(1).0).await;

        let slot = 42;
        let block_hash1 = B256::try_from([4u8; 32].as_ref()).unwrap();
        let block_hash2 = B256::try_from([5u8; 32].as_ref()).unwrap();

        // Set initial slot and hash
        assert!(cache.check_and_set_last_slot_and_hash_delivered(slot, &block_hash1).is_ok());

        // Test: Set the same slot with a different hash
        let set_result = cache.check_and_set_last_slot_and_hash_delivered(slot, &block_hash2);
        assert!(matches!(set_result, Err(AuctioneerError::AnotherPayloadAlreadyDeliveredForSlot)));
    }

    #[tokio::test]
    async fn test_get_and_save_execution_payload() {
        let cache = LocalCache::new(Vec::new(), crossbeam_channel::bounded(1).0).await;

        let slot = 42;
        let proposer_pub_key = BlsPublicKey::test_random();
        let block_hash = B256::test_random();

        let payload = ExecutionPayloadElectra { gas_limit: 999, ..Default::default() };
        let blobs_bundle = BlobsBundle::default();
        let versioned_execution_payload = PayloadAndBlobsRef {
            execution_payload: ExecutionPayloadRef::Electra(&payload),
            blobs_bundle: &blobs_bundle,
        };

        // Save the execution payload
        cache.save_execution_payload(
            slot,
            &proposer_pub_key,
            &block_hash,
            versioned_execution_payload,
        );

        // Test: Get the execution payload
        let get_result: Option<PayloadAndBlobs> =
            cache.get_execution_payload(slot, &proposer_pub_key, &block_hash, ForkName::Electra);
        assert!(get_result.as_ref().is_some(), "Execution payload is None");

        let fetched_execution_payload = get_result.unwrap();
        assert_eq!(
            fetched_execution_payload.execution_payload.gas_limit(),
            999,
            "Execution payload mismatch"
        );
    }

    #[tokio::test]
    async fn test_get_builder_info() {
        let cache = LocalCache::new(Vec::new(), crossbeam_channel::bounded(1).0).await;

        let builder_pub_key = BlsPublicKey::test_random();
        let unknown_builder_pub_key = BlsPublicKey::test_random();

        let builder_info = BuilderInfo {
            collateral: U256::from(12),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: false,
            builder_id: None,
            builder_ids: None,
        };

        // Test case 1: Builder exists
        let builder_info_doc =
            BuilderConfig { pub_key: builder_pub_key.clone(), builder_info: builder_info.clone() };
        cache.update_builder_infos(&[builder_info_doc]);

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
    async fn test_get_trusted_proposers_and_update_trusted_proposers() {
        let cache = LocalCache::new(Vec::new(), crossbeam_channel::bounded(1).0).await;

        let is_trusted = cache.is_trusted_proposer(&BlsPublicKey::test_random());
        assert!(!is_trusted, "Failed to check trusted proposer");

        cache.update_trusted_proposers(vec![
            ProposerInfo { name: "test".to_string(), pubkey: get_fixed_pubkey(0) },
            ProposerInfo { name: "test2".to_string(), pubkey: get_fixed_pubkey(1) },
        ]);

        let is_trusted = cache.is_trusted_proposer(&get_fixed_pubkey(0));
        assert!(is_trusted, "Failed to check trusted proposer");

        let is_trusted = cache.is_trusted_proposer(&get_fixed_pubkey(1));
        assert!(is_trusted, "Failed to check trusted proposer");

        let is_trusted = cache.is_trusted_proposer(&get_fixed_pubkey(2));
        assert!(!is_trusted, "Failed to check trusted proposer");

        cache.update_trusted_proposers(vec![ProposerInfo {
            name: "test2".to_string(),
            pubkey: get_fixed_pubkey(3),
        }]);

        let is_trusted = cache.is_trusted_proposer(&BlsPublicKey::test_random());
        assert!(!is_trusted, "Failed to check trusted proposer");

        let is_trusted = cache.is_trusted_proposer(&get_fixed_pubkey(3));
        assert!(is_trusted, "Failed to check trusted proposer");
    }

    #[tokio::test]
    async fn test_demote_non_optimistic_builder() {
        let cache = LocalCache::new(Vec::new(), crossbeam_channel::bounded(1).0).await;

        let builder_pub_key = BlsPublicKey::test_random();
        let builder_info = BuilderInfo {
            collateral: U256::from(12),
            is_optimistic: false,
            is_optimistic_for_regional_filtering: false,
            builder_id: None,
            builder_ids: None,
        };

        let builder_info_doc = BuilderConfig { pub_key: builder_pub_key.clone(), builder_info };

        // Set builder info in the cache
        cache.update_builder_infos(&[builder_info_doc]);

        // Test: Demote builder
        let result = cache.demote_builder(&builder_pub_key);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_demote_optimistic_builder() {
        let cache = LocalCache::new(Vec::new(), crossbeam_channel::bounded(1).0).await;

        let builder_pub_key_optimistic = BlsPublicKey::test_random();
        let builder_info = BuilderInfo {
            collateral: U256::from(12),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: false,
            builder_id: None,
            builder_ids: None,
        };
        let builder_info_doc =
            BuilderConfig { pub_key: builder_pub_key_optimistic.clone(), builder_info };

        // Set builder info in the cache
        cache.update_builder_infos(&[builder_info_doc]);

        assert!(
            cache.get_builder_info(&builder_pub_key_optimistic).unwrap().is_optimistic,
            "Builder is not optimistic after setting"
        );

        // Test: Demote builder
        let result = cache.demote_builder(&builder_pub_key_optimistic);
        assert!(result.is_ok());

        // Validate: builder is no longer optimistic
        assert!(!cache.get_builder_info(&builder_pub_key_optimistic).unwrap().is_optimistic);
    }

    #[tokio::test]
    async fn test_seen_or_insert_block_hash() {
        let cache = LocalCache::new(Vec::new(), crossbeam_channel::bounded(1).0).await;

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
    async fn test_kill_switch() {
        let cache = LocalCache::new(Vec::new(), crossbeam_channel::bounded(1).0).await;

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
