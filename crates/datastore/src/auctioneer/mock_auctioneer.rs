use std::sync::{atomic::AtomicBool, Arc, Mutex};

use alloy_primitives::B256;
use helix_common::{
    api::builder_api::{
        BuilderGetValidatorsResponseEntry, InclusionListWithKey, InclusionListWithMetadata,
        SlotCoordinate,
    },
    BuilderInfo, ProposerInfo,
};
use helix_database::types::BuilderInfoDocument;
use helix_types::{BlsPublicKey, ForkName, PayloadAndBlobs, SignedBuilderBid, TestRandomSeed};
use http::HeaderValue;
use tokio::sync::broadcast;

use crate::{error::AuctioneerError, Auctioneer};

#[derive(Default, Clone)]
pub struct MockAuctioneer {
    pub builder_info: Option<BuilderInfo>,
    pub builder_demoted: Arc<AtomicBool>,
    pub kill_switch_enabled: Arc<AtomicBool>,
    pub best_bid: Arc<Mutex<Option<SignedBuilderBid>>>,
    pub versioned_execution_payload: Arc<Mutex<Option<PayloadAndBlobs>>>,
}

impl MockAuctioneer {
    pub fn new() -> Self {
        Self {
            builder_info: None,
            builder_demoted: Arc::new(AtomicBool::new(false)),
            kill_switch_enabled: Arc::new(AtomicBool::new(false)),
            best_bid: Arc::new(Mutex::new(None)),
            versioned_execution_payload: Arc::new(Mutex::new(None)),
        }
    }
}

impl Auctioneer for MockAuctioneer {
    fn get_last_slot_delivered(&self) -> Option<u64> {
        None
    }
    fn check_and_set_last_slot_and_hash_delivered(
        &self,
        _slot: u64,
        _hash: &B256,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    fn save_execution_payload(
        &self,
        _slot: u64,
        _proposer_pub_key: &BlsPublicKeyBytes,
        _block_hash: &B256,
        _execution_payload: PayloadAndBlobs,
    ) {
    }
    fn get_execution_payload(
        &self,
        _slot: u64,
        _proposer_pub_key: &BlsPublicKeyBytes,
        _block_hash: &B256,
        _fork_name: ForkName,
    ) -> Option<PayloadAndBlobs> {
        self.versioned_execution_payload.lock().unwrap().clone()
    }

    fn get_builder_info(
        &self,
        _builder_pub_key: &BlsPublicKeyBytes,
    ) -> Result<BuilderInfo, AuctioneerError> {
        Ok(self.builder_info.clone().unwrap_or_default())
    }

    fn contains_api_key(&self, _api_key: &HeaderValue) -> bool {
        self.builder_info.is_some()
    }

    fn validate_api_key(&self, _api_key: &HeaderValue, _pubkey: &BlsPublicKeyBytes) -> bool {
        self.builder_info.is_some()
    }

    fn demote_builder(&self, _builder_pub_key: &BlsPublicKeyBytes) -> Result<(), AuctioneerError> {
        self.builder_demoted.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn update_builder_infos(&self, _builder_infos: &[BuilderInfoDocument]) {}

    fn seen_or_insert_block_hash(&self, _block_hash: &B256) -> bool {
        false
    }

    fn update_trusted_proposers(&self, _proposer_whitelist: Vec<ProposerInfo>) {}

    fn is_trusted_proposer(&self, _proposer_pub_key: &BlsPublicKeyBytes) -> bool {
        true
    }

    fn update_primev_proposers(&self, _proposer_whitelist: &[BlsPublicKeyBytes]) {}

    fn is_primev_proposer(&self, _proposer_pub_key: &BlsPublicKeyBytes) -> bool {
        true
    }

    fn kill_switch_enabled(&self) -> bool {
        self.kill_switch_enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn enable_kill_switch(&self) {
        self.kill_switch_enabled.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn disable_kill_switch(&self) {
        self.kill_switch_enabled.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn save_payload_address(
        &self,
        _block_hash: &B256,
        _builder_pubkey: &BlsPublicKeyBytes,
        _payload_socket_address: Vec<u8>,
    ) {
    }

    fn get_payload_url(&self, _block_hash: &B256) -> Option<(BlsPublicKeyBytes, Vec<u8>)> {
        None
    }

    fn update_current_inclusion_list(&self, _: InclusionListWithMetadata, _: SlotCoordinate) {}

    fn get_inclusion_list(&self) -> broadcast::Receiver<InclusionListWithKey> {
        let (tx, rx) = broadcast::channel(1);
        tx.send(InclusionListWithKey {
            key: (0, BlsPublicKeyBytes::random(), B256::default()),
            inclusion_list: InclusionListWithMetadata { txs: vec![] },
        })
        .unwrap();
        rx
    }

    fn update_proposer_duties(&self, _duties: Vec<BuilderGetValidatorsResponseEntry>) {}

    fn get_proposer_duties(&self) -> Vec<BuilderGetValidatorsResponseEntry> {
        vec![]
    }

    fn process_slot(&self, _head_slot: u64) {}
}
