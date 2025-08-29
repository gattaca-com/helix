use alloy_primitives::B256;
use async_trait::async_trait;
use helix_beacon::types::{HeadEventData, PayloadAttributesEvent};
use helix_common::{
    api::builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
    builder_info::BuilderInfo,
    ProposerInfo,
};
use helix_database::BuilderInfoDocument;
use helix_types::{BidTrace, BlsPublicKey, ForkName, PayloadAndBlobs, PayloadAndBlobsRef};
use tokio::sync::broadcast;

use crate::{error::AuctioneerError, redis::redis_cache::InclusionListWithKey};

#[async_trait]
#[auto_impl::auto_impl(Arc)]
pub trait Auctioneer: Send + Sync + Clone {
    async fn get_last_slot_delivered(&self) -> Result<Option<u64>, AuctioneerError>;
    async fn check_and_set_last_slot_and_hash_delivered(
        &self,
        slot: u64,
        hash: &B256,
    ) -> Result<(), AuctioneerError>;

    async fn save_execution_payload<'a>(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &B256,
        versioned_execution_payload: PayloadAndBlobsRef<'a>,
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

    async fn get_builder_info(
        &self,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<BuilderInfo, AuctioneerError>;
    async fn demote_builder(&self, builder_pub_key: &BlsPublicKey) -> Result<(), AuctioneerError>;

    async fn update_builder_infos(
        &self,
        builder_infos: &[BuilderInfoDocument],
    ) -> Result<(), AuctioneerError>;

    async fn seen_or_insert_block_hash(&self, block_hash: &B256) -> Result<bool, AuctioneerError>;

    async fn update_trusted_proposers(
        &self,
        proposer_whitelist: Vec<ProposerInfo>,
    ) -> Result<(), AuctioneerError>;

    async fn is_trusted_proposer(
        &self,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<bool, AuctioneerError>;

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

    async fn kill_switch_enabled(&self) -> Result<bool, AuctioneerError>;

    async fn enable_kill_switch(&self) -> Result<(), AuctioneerError>;

    async fn disable_kill_switch(&self) -> Result<(), AuctioneerError>;

    async fn update_current_inclusion_list(
        &self,
        inclusion_list: InclusionListWithMetadata,
        slot_coordinate: String,
    ) -> Result<(), AuctioneerError>;

    fn get_inclusion_list(&self) -> broadcast::Receiver<InclusionListWithKey>;

    async fn publish_head_event(&self, head_event: &HeadEventData) -> Result<(), AuctioneerError>;

    fn get_head_event(&self) -> broadcast::Receiver<HeadEventData>;

    async fn publish_payload_attributes(
        &self,
        payload_attributes_event: &PayloadAttributesEvent,
    ) -> Result<(), AuctioneerError>;

    fn get_payload_attributes(&self) -> broadcast::Receiver<PayloadAttributesEvent>;

    async fn update_proposer_duties(
        &self,
        duties: Vec<BuilderGetValidatorsResponseEntry>,
    ) -> Result<(), AuctioneerError>;

    async fn get_proposer_duties(
        &self,
    ) -> Result<Vec<BuilderGetValidatorsResponseEntry>, AuctioneerError>;
}
