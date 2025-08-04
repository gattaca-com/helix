use std::sync::{atomic::AtomicBool, Arc, Mutex};

use alloy_primitives::B256;
use async_trait::async_trait;
use helix_beacon::types::PayloadAttributesEvent;
use helix_common::{
    api::builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
    BuilderInfo, ProposerInfo,
};
use helix_database::types::BuilderInfoDocument;
use helix_types::{
    BidTrace, BlockMergingPreferences, BlsPublicKey, ForkName, PayloadAndBlobs, PayloadAndBlobsRef,
    SignedBuilderBid,
};
use tokio::sync::broadcast;

use crate::{error::AuctioneerError, redis::redis_cache::InclusionListWithKey, Auctioneer};

#[derive(Default, Clone)]
pub struct MockAuctioneer {
    pub builder_info: Option<BuilderInfo>,
    pub builder_demoted: Arc<AtomicBool>,
    pub best_bid: Arc<Mutex<Option<SignedBuilderBid>>>,
    pub versioned_execution_payload: Arc<Mutex<Option<PayloadAndBlobs>>>,
}

impl MockAuctioneer {
    pub fn new() -> Self {
        Self {
            builder_info: None,
            builder_demoted: Arc::new(AtomicBool::new(false)),
            best_bid: Arc::new(Mutex::new(None)),
            versioned_execution_payload: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait]
impl Auctioneer for MockAuctioneer {
    async fn get_last_slot_delivered(&self) -> Result<Option<u64>, AuctioneerError> {
        Ok(None)
    }
    async fn check_and_set_last_slot_and_hash_delivered(
        &self,
        _slot: u64,
        _hash: &B256,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn save_execution_payload<'a>(
        &self,
        _slot: u64,
        _proposer_pub_key: &BlsPublicKey,
        _block_hash: &B256,
        _execution_payload: PayloadAndBlobsRef<'a>,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }
    async fn get_execution_payload(
        &self,
        _slot: u64,
        _proposer_pub_key: &BlsPublicKey,
        _block_hash: &B256,
        _fork_name: ForkName,
    ) -> Result<Option<PayloadAndBlobs>, AuctioneerError> {
        Ok(self.versioned_execution_payload.lock().unwrap().clone())
    }

    async fn get_bid_trace(
        &self,
        _slot: u64,
        _proposer_pub_key: &BlsPublicKey,
        _block_hash: &B256,
    ) -> Result<Option<BidTrace>, AuctioneerError> {
        Ok(None)
    }
    async fn save_bid_trace(&self, _bid_trace: &BidTrace) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn get_block_merging_preferences(
        &self,
        _slot: u64,
        _proposer_pub_key: &BlsPublicKey,
        _block_hash: &B256,
    ) -> Result<Option<BlockMergingPreferences>, AuctioneerError> {
        Ok(None)
    }
    async fn save_block_merging_preferences(
        &self,
        _slot: u64,
        _proposer_pub_key: &BlsPublicKey,
        _block_hash: &B256,
        _merging_data: &BlockMergingPreferences,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn get_builder_info(
        &self,
        _builder_pub_key: &BlsPublicKey,
    ) -> Result<BuilderInfo, AuctioneerError> {
        Ok(self.builder_info.clone().unwrap_or_default())
    }

    async fn demote_builder(&self, _builder_pub_key: &BlsPublicKey) -> Result<(), AuctioneerError> {
        self.builder_demoted.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    async fn update_builder_infos(
        &self,
        _builder_infos: &[BuilderInfoDocument],
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn seen_or_insert_block_hash(&self, _block_hash: &B256) -> Result<bool, AuctioneerError> {
        Ok(false)
    }

    async fn update_trusted_proposers(
        &self,
        _proposer_whitelist: Vec<ProposerInfo>,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn is_trusted_proposer(
        &self,
        _proposer_pub_key: &BlsPublicKey,
    ) -> Result<bool, AuctioneerError> {
        Ok(true)
    }

    async fn try_acquire_or_renew_leadership(&self, _leader_id: &str) -> bool {
        true
    }

    async fn update_primev_proposers(
        &self,
        _proposer_whitelist: &[BlsPublicKey],
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn is_primev_proposer(
        &self,
        _proposer_pub_key: &BlsPublicKey,
    ) -> Result<bool, AuctioneerError> {
        Ok(true)
    }

    async fn kill_switch_enabled(&self) -> Result<bool, AuctioneerError> {
        Ok(false)
    }

    async fn enable_kill_switch(&self) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn disable_kill_switch(&self) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn save_payload_address(
        &self,
        _block_hash: &B256,
        _builder_pubkey: &BlsPublicKey,
        _payload_socket_address: Vec<u8>,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn get_payload_url(
        &self,
        _block_hash: &B256,
    ) -> Result<Option<(BlsPublicKey, Vec<u8>)>, AuctioneerError> {
        Ok(None)
    }

    async fn update_current_inclusion_list(
        &self,
        _: InclusionListWithMetadata,
        _: String,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    fn get_inclusion_list(&self) -> broadcast::Receiver<InclusionListWithKey> {
        let (tx, rx) = broadcast::channel(1);
        tx.send(InclusionListWithKey {
            key: "".into(),
            inclusion_list: InclusionListWithMetadata { txs: vec![] },
        })
        .unwrap();
        rx
    }

    async fn publish_head_event(
        &self,
        _head_event: &helix_beacon::types::HeadEventData,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    fn get_head_event(&self) -> broadcast::Receiver<helix_beacon::types::HeadEventData> {
        let (tx, rx) = broadcast::channel(1);
        tx.send(helix_beacon::types::HeadEventData::default()).unwrap();
        rx
    }

    async fn publish_payload_attributes(
        &self,
        _payload_attributes: &PayloadAttributesEvent,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    fn get_payload_attributes(&self) -> broadcast::Receiver<PayloadAttributesEvent> {
        let (tx, rx) = broadcast::channel(1);
        tx.send(PayloadAttributesEvent::default()).unwrap();
        rx
    }

    async fn update_proposer_duties(
        &self,
        _duties: Vec<BuilderGetValidatorsResponseEntry>,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn get_proposer_duties(
        &self,
    ) -> Result<Vec<BuilderGetValidatorsResponseEntry>, AuctioneerError> {
        Ok(vec![])
    }
}
