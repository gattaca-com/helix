use async_trait::async_trait;
use ethereum_consensus::primitives::{BlsPublicKey, Hash32, U256};
use helix_database::BuilderInfoDocument;
use helix_common::{
    bid_submission::{BidTrace, SignedBidSubmission, v2::header_submission::SignedHeaderSubmission}, builder_info::BuilderInfo, eth::SignedBuilderBid, signing::RelaySigningContext, BuilderID, versioned_payload::PayloadAndBlobs, ProposerInfo, ProposerInfoSet
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
        versioned_execution_payload: &PayloadAndBlobs,
    ) -> Result<(), AuctioneerError>;
    async fn get_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &Hash32,
    ) -> Result<Option<PayloadAndBlobs>, AuctioneerError>;

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

    async fn seen_or_insert_block_hash(
        &self,
        block_hash: &Hash32,
        slot: u64,
        parent_hash: &Hash32,
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

    async fn get_trusted_proposers(&self) -> Result<Option<ProposerInfoSet>, AuctioneerError>;
}
