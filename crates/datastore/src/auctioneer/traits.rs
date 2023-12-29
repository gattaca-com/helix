use std::sync::Arc;

use async_trait::async_trait;
use ethereum_consensus::{
    primitives::{BlsPublicKey, Hash32, U256},
    types::mainnet::ExecutionPayload,
};
use helix_database::BuilderInfoDocument;
use helix_common::{
    bid_submission::{BidTrace, SignedBidSubmission},
    builder_info::BuilderInfo,
    eth::SignedBuilderBid,
    signing::RelaySigningContext,
};

use crate::{error::AuctioneerError, types::SaveBidAndUpdateTopBidResponse};

#[async_trait]
#[auto_impl::auto_impl(Arc)]
pub trait Auctioneer: Send + Sync + Clone {
    async fn get_last_slot_delivered(&self) -> Result<Option<u64>, AuctioneerError>;
    async fn check_and_set_last_slot_and_hash_delivered(
        &self,
        slot: u64,
        hash: &Hash32,
    ) -> Result<(), AuctioneerError>;

    async fn get_best_bid(
        &self,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<SignedBuilderBid>, AuctioneerError>;

    async fn save_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &Hash32,
        execution_payload: &ExecutionPayload,
    ) -> Result<(), AuctioneerError>;
    async fn get_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &Hash32,
    ) -> Result<Option<ExecutionPayload>, AuctioneerError>;

    async fn get_bid_trace(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &Hash32,
    ) -> Result<Option<BidTrace>, AuctioneerError>;
    async fn save_bid_trace(&self, bid_trace: &BidTrace) -> Result<(), AuctioneerError>;

    async fn get_builder_latest_payload_received_at(
        &self,
        slot: u64,
        builder_pub_key: &BlsPublicKey,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<u64>, AuctioneerError>;

    async fn save_builder_bid(
        &self,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
        builder_pub_key: &BlsPublicKey,
        received_at: u128,
        builder_bid: SignedBuilderBid,
    ) -> Result<(), AuctioneerError>;

    async fn save_bid_and_update_top_bid(
        &self,
        submission: Arc<SignedBidSubmission>,
        received_at: u128,
        cancellations_enabled: bool,
        floor_value: U256,
        state: &mut SaveBidAndUpdateTopBidResponse,
        signing_context: &RelaySigningContext,
    ) -> Result<(), AuctioneerError>;

    async fn get_top_bid_value(
        &self,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<U256>, AuctioneerError>;
    async fn get_builder_latest_value(
        &self,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<Option<U256>, AuctioneerError>;
    async fn get_floor_bid_value(
        &self,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<U256>, AuctioneerError>;

    async fn delete_builder_bid(
        &self,
        slot: u64,
        parent_hash: &Hash32,
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
}
