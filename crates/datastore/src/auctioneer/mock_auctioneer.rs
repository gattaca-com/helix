use std::{
    collections::HashMap,
    sync::{atomic::AtomicBool, Arc, Mutex},
};

use async_trait::async_trait;
use ethereum_consensus::primitives::{BlsPublicKey, Hash32, U256};

use helix_common::{
    api::constraints_api::{SignedDelegation, SignedRevocation},
    bellatrix::Node,
    bid_submission::{
        v2::header_submission::SignedHeaderSubmission, BidTrace, SignedBidSubmission,
    },
    eth::SignedBuilderBid,
    pending_block::PendingBlock,
    proofs::{InclusionProofs, SignedConstraintsWithProofData},
    signing::RelaySigningContext,
    versioned_payload::PayloadAndBlobs,
    BuilderInfo, ProposerInfo,
};
use helix_database::types::BuilderInfoDocument;
use tokio_stream::Stream;

use crate::{error::AuctioneerError, types::SaveBidAndUpdateTopBidResponse, Auctioneer};

#[derive(Default, Clone)]
pub struct MockAuctioneer {
    pub builder_info: Option<BuilderInfo>,
    pub builder_demoted: Arc<AtomicBool>,
    pub best_bid: Arc<Mutex<Option<SignedBuilderBid>>>,
    pub versioned_execution_payload: Arc<Mutex<Option<PayloadAndBlobs>>>,
    pub constraints: Arc<Mutex<HashMap<u64, Vec<SignedConstraintsWithProofData>>>>,
}

impl MockAuctioneer {
    pub fn new() -> Self {
        Self {
            builder_info: None,
            builder_demoted: Arc::new(AtomicBool::new(false)),
            best_bid: Arc::new(Mutex::new(None)),
            versioned_execution_payload: Arc::new(Mutex::new(None)),
            constraints: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl Auctioneer for MockAuctioneer {
    async fn get_validator_delegations(
        &self,
        _pub_key: BlsPublicKey,
    ) -> Result<Vec<SignedDelegation>, AuctioneerError> {
        Ok(vec![])
    }

    async fn save_validator_delegations(
        &self,
        _signed_delegations: Vec<SignedDelegation>,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn revoke_validator_delegations(
        &self,
        _signed_revocations: Vec<SignedRevocation>,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn save_constraints(
        &self,
        slot: u64,
        constraints: SignedConstraintsWithProofData,
    ) -> Result<(), AuctioneerError> {
        let mut constraints_map = self.constraints.lock().unwrap();
        let constraints_vec = constraints_map.entry(slot).or_insert_with(Vec::new);
        constraints_vec.push(constraints);
        Ok(())
    }
    async fn get_constraints(
        &self,
        slot: u64,
    ) -> Result<Option<Vec<SignedConstraintsWithProofData>>, AuctioneerError> {
        let temp = self.constraints.lock().unwrap();
        let constraints = temp.get(&slot);
        match constraints {
            Some(constraints) => Ok(Some(constraints.to_vec())),
            None => Ok(None),
        }
    }

    async fn save_inclusion_proof(
        &self,
        _slot: u64,
        _proposer_pub_key: &BlsPublicKey,
        _bid_block_hash: &Hash32,
        _inclusion_proof: &InclusionProofs,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }
    async fn get_inclusion_proof(
        &self,
        _slot: u64,
        _proposer_pub_key: &BlsPublicKey,
        _bid_block_hash: &Hash32,
    ) -> Result<Option<InclusionProofs>, AuctioneerError> {
        Ok(None)
    }

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
                return Err(AuctioneerError::UnexpectedValueType)
            }
        }
        Ok(self.best_bid.lock().unwrap().clone())
    }
    async fn get_best_bids(
        &self,
    ) -> Box<dyn Stream<Item = Result<Vec<u8>, AuctioneerError>> + Send + Unpin> {
        Box::new(tokio_stream::iter(vec![Ok(vec![0; 188]), Ok(vec![0; 188]), Ok(vec![0; 188])]))
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

    async fn get_pending_blocks(&self) -> Result<Vec<PendingBlock>, AuctioneerError> {
        Ok(vec![])
    }

    async fn save_pending_block_header(
        &self,
        _slot: u64,
        _builder_pub_key: &BlsPublicKey,
        _block_hash: &Hash32,
        _timestamp_ms: u64,
    ) -> Result<(), AuctioneerError> {
        Ok(())
    }

    async fn save_pending_block_payload(
        &self,
        _slot: u64,
        _builder_pub_key: &BlsPublicKey,
        _block_hash: &Hash32,
        _timestamp_ms: u64,
    ) -> Result<(), AuctioneerError> {
        Ok(())
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

    async fn get_header_tx_root(
        &self,
        _block_hash: &Hash32,
    ) -> Result<Option<Node>, AuctioneerError> {
        Ok(None)
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
}
