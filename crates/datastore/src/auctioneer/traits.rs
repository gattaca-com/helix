use alloy_primitives::{bytes::Bytes, B256, U256};
use async_trait::async_trait;
use helix_common::{
    api::builder_api::InclusionListWithMetadata,
    bid_submission::v2::header_submission::SignedHeaderSubmission, builder_info::BuilderInfo,
    pending_block::PendingBlock, signing::RelaySigningContext, ProposerInfo,
};
use helix_database::BuilderInfoDocument;
use helix_types::{
    BidTrace, BlsPublicKey, ForkName, PayloadAndBlobs, SignedBidSubmission, SignedBuilderBid,
};
use tokio::sync::broadcast;

use crate::{
    error::AuctioneerError, redis::redis_cache::InclusionListWithKey,
    types::SaveBidAndUpdateTopBidResponse,
};

#[async_trait]
#[auto_impl::auto_impl(Arc)]
pub trait Auctioneer: Send + Sync + Clone {
    async fn get_last_slot_delivered(&self) -> Result<Option<u64>, AuctioneerError>;
    async fn check_and_set_last_slot_and_hash_delivered(
        &self,
        slot: u64,
        hash: &B256,
    ) -> Result<(), AuctioneerError>;

    async fn get_best_bid(
        &self,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<SignedBuilderBid>, AuctioneerError>;

    fn get_best_bids(&self) -> broadcast::Receiver<Bytes>;

    async fn save_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &B256,
        versioned_execution_payload: &PayloadAndBlobs,
    ) -> Result<(), AuctioneerError>;
    async fn get_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &B256,
        fork_name: ForkName,
    ) -> Result<Option<PayloadAndBlobs>, AuctioneerError>;

    async fn get_bid_trace(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &B256,
    ) -> Result<Option<BidTrace>, AuctioneerError>;
    async fn save_bid_trace(&self, bid_trace: &BidTrace) -> Result<(), AuctioneerError>;

    async fn get_builder_latest_payload_received_at(
        &self,
        slot: u64,
        builder_pub_key: &BlsPublicKey,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<u64>, AuctioneerError>;

    async fn save_builder_bid(
        &self,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
        builder_pub_key: &BlsPublicKey,
        received_at: u128,
        builder_bid: &SignedBuilderBid,
    ) -> Result<(), AuctioneerError>;

    async fn save_bid_and_update_top_bid(
        &self,
        submission: &SignedBidSubmission,
        received_at: u128,
        cancellations_enabled: bool,
        floor_value: U256,
        state: &mut SaveBidAndUpdateTopBidResponse,
        signing_context: &RelaySigningContext,
    ) -> Result<Option<(SignedBuilderBid, PayloadAndBlobs)>, AuctioneerError>;

    async fn get_top_bid_value(
        &self,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<U256>, AuctioneerError>;
    async fn get_builder_latest_value(
        &self,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<Option<U256>, AuctioneerError>;
    async fn get_floor_bid_value(
        &self,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<U256>, AuctioneerError>;

    async fn delete_builder_bid(
        &self,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<(), AuctioneerError>;

    async fn get_builder_info(
        &self,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<BuilderInfo, AuctioneerError>;
    async fn demote_builder(&self, builder_pub_key: &BlsPublicKey) -> Result<(), AuctioneerError>;

    async fn update_builder_infos(
        &self,
        builder_infos: Vec<BuilderInfoDocument>,
    ) -> Result<(), AuctioneerError>;

    async fn seen_or_insert_block_hash(
        &self,
        block_hash: &B256,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<bool, AuctioneerError>;

    async fn save_signed_builder_bid_and_update_top_bid(
        &self,
        builder_bid: &SignedBuilderBid,
        bid_trace: &BidTrace,
        received_at: u128,
        cancellations_enabled: bool,
        floor_value: U256,
        state: &mut SaveBidAndUpdateTopBidResponse,
    ) -> Result<(), AuctioneerError>;

    async fn save_header_submission_and_update_top_bid(
        &self,
        submission: &SignedHeaderSubmission,
        received_at: u128,
        cancellations_enabled: bool,
        floor_value: U256,
        state: &mut SaveBidAndUpdateTopBidResponse,
        signing_context: &RelaySigningContext,
    ) -> Result<Option<SignedBuilderBid>, AuctioneerError>;

    async fn update_trusted_proposers(
        &self,
        proposer_whitelist: Vec<ProposerInfo>,
    ) -> Result<(), AuctioneerError>;

    async fn is_trusted_proposer(
        &self,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<bool, AuctioneerError>;

    async fn get_pending_blocks(&self) -> Result<Vec<PendingBlock>, AuctioneerError>;

    async fn save_pending_block_header(
        &self,
        slot: u64,
        builder_pub_key: &BlsPublicKey,
        block_hash: &B256,
        timestamp_ms: u64,
    ) -> Result<(), AuctioneerError>;

    async fn save_pending_block_payload(
        &self,
        slot: u64,
        builder_pub_key: &BlsPublicKey,
        block_hash: &B256,
        timestamp_ms: u64,
    ) -> Result<(), AuctioneerError>;

    async fn save_payload_address(
        &self,
        block_hash: &B256,
        builder_pub_key: &BlsPublicKey,
        payload_url: Vec<u8>,
    ) -> Result<(), AuctioneerError>;

    async fn get_payload_url(
        &self,
        block_hash: &B256,
    ) -> Result<Option<(BlsPublicKey, Vec<u8>)>, AuctioneerError>;

    /// Try to acquire or renew leadership for the housekeeper.
    /// Returns: true if the housekeeper is the leader, false if it isn't.
    async fn try_acquire_or_renew_leadership(&self, leader_id: &str) -> bool;

    async fn update_primev_proposers(
        &self,
        proposer_whitelist: &[BlsPublicKey],
    ) -> Result<(), AuctioneerError>;

    async fn is_primev_proposer(
        &self,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<bool, AuctioneerError>;

    async fn get_header_tx_root(&self, block_hash: &B256) -> Result<Option<B256>, AuctioneerError>;

    async fn kill_switch_enabled(&self) -> Result<bool, AuctioneerError>;

    async fn enable_kill_switch(&self) -> Result<(), AuctioneerError>;

    async fn disable_kill_switch(&self) -> Result<(), AuctioneerError>;

    async fn update_current_inclusion_list(
        &self,
        inclusion_list: InclusionListWithMetadata,
        slot_coordinate: String,
    ) -> Result<(), AuctioneerError>;

    fn get_inclusion_list(&self) -> broadcast::Receiver<InclusionListWithKey>;
}
