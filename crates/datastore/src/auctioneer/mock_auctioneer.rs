use std::sync::{atomic::AtomicBool, Arc, Mutex};

use async_trait::async_trait;
use ethereum_consensus::primitives::{BlsPublicKey, Hash32, U256};

use helix_common::versioned_payload::PayloadAndBlobs;
use helix_common::ProposerInfo;
use helix_database::types::BuilderInfoDocument;
use helix_common::{signing::RelaySigningContext, bid_submission::v2::header_submission::SignedHeaderSubmission};
use helix_common::{
    bid_submission::{BidTrace, SignedBidSubmission},
    eth::SignedBuilderBid,
    BuilderInfo,
};

use crate::{error::AuctioneerError, types::SaveBidAndUpdateTopBidResponse, Auctioneer};

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
        _hash: &Hash32,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn get_best_bid(
        &self,
        _slot: u64,
        _parent_hash: &Hash32,
        _proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<SignedBuilderBid>, AuctioneerError> {
        // check if the value is 9999 and than return an error for testing
        if let Some(bid) = self.best_bid.lock().unwrap().clone() {
            if bid.value() == U256::from(9999) {
                return Err(AuctioneerError::UnexpectedValueType);
            }
        }
        Ok(self.best_bid.lock().unwrap().clone())
    }

    async fn save_execution_payload(
        &self,
        _slot: u64,
        _proposer_pub_key: &BlsPublicKey,
        _block_hash: &Hash32,
        _execution_payload: &PayloadAndBlobs,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }
    async fn get_execution_payload(
        &self,
        _slot: u64,
        _proposer_pub_key: &BlsPublicKey,
        _block_hash: &Hash32,
    ) -> Result<Option<PayloadAndBlobs>, AuctioneerError> {
        Ok(self.versioned_execution_payload.lock().unwrap().clone())
    }

    async fn get_bid_trace(
        &self,
        _slot: u64,
        _proposer_pub_key: &BlsPublicKey,
        _block_hash: &Hash32,
    ) -> Result<Option<BidTrace>, AuctioneerError> {
        Ok(None)
    }
    async fn save_bid_trace(&self, _bid_trace: &BidTrace) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn get_builder_latest_payload_received_at(
        &self,
        _slot: u64,
        _builder_pub_key: &BlsPublicKey,
        _parent_hash: &Hash32,
        _proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<u64>, AuctioneerError> {
        Ok(None)
    }

    async fn save_builder_bid(
        &self,
        _slot: u64,
        _parent_hash: &Hash32,
        _proposer_pub_key: &BlsPublicKey,
        _builder_pub_key: &BlsPublicKey,
        _received_at: u128,
        _builder_bid: &SignedBuilderBid,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn save_bid_and_update_top_bid(
        &self,
        _submission: &SignedBidSubmission,
        _received_at: u128,
        _cancellations_enabled: bool,
        _floor_value: U256,
        _state: &mut SaveBidAndUpdateTopBidResponse,
        _signing_context: &RelaySigningContext,
    ) -> Result<Option<(SignedBuilderBid, PayloadAndBlobs)>, AuctioneerError> {
        Ok(None)
    }

    async fn get_top_bid_value(
        &self,
        _slot: u64,
        _parent_hash: &Hash32,
        _proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<U256>, AuctioneerError> {
        Ok(None)
    }
    async fn get_builder_latest_value(
        &self,
        _slot: u64,
        _parent_hash: &Hash32,
        _proposer_pub_key: &BlsPublicKey,
        _builder_pub_key: &BlsPublicKey,
    ) -> Result<Option<U256>, AuctioneerError> {
        Ok(None)
    }
    async fn get_floor_bid_value(
        &self,
        _slot: u64,
        _parent_hash: &Hash32,
        _proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<U256>, AuctioneerError> {
        Ok(None)
    }

    async fn delete_builder_bid(
        &self,
        _slot: u64,
        _parent_hash: &Hash32,
        _proposer_pub_key: &BlsPublicKey,
        _builder_pub_key: &BlsPublicKey,
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
        _builder_infos: Vec<BuilderInfoDocument>,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn seen_or_insert_block_hash(
        &self,
        _block_hash: &Hash32,
        _slot: u64,
        _parent_hash: &Hash32,
        _proposer_pub_key: &BlsPublicKey,
    ) -> Result<bool, AuctioneerError> {
        Ok(false)
    }

    async fn save_signed_builder_bid_and_update_top_bid(
        &self,
        _builder_bid: &SignedBuilderBid,
        _bid_trace: &BidTrace,
        _received_at: u128,
        __cancellations_enabled: bool,
        _floor_value: U256,
        _state: &mut SaveBidAndUpdateTopBidResponse,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn save_header_submission_and_update_top_bid(
        &self,
        _submission: &SignedHeaderSubmission,
        _received_at: u128,
        _cancellations_enabled: bool,
        _floor_value: U256,
        _state: &mut SaveBidAndUpdateTopBidResponse,
        _signing_context: &RelaySigningContext,
    ) -> Result<Option<SignedBuilderBid>, AuctioneerError> {
        Ok(None)
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
}
