use alloy_primitives::B256;
use helix_common::{
    api::builder_api::{
        BuilderGetValidatorsResponseEntry, InclusionListWithKey, InclusionListWithMetadata,
        SlotCoordinate,
    },
    builder_info::BuilderInfo,
    ProposerInfo,
};
use helix_database::BuilderInfoDocument;
use helix_types::{BlsPublicKey, ForkName, PayloadAndBlobs};
use http::HeaderValue;
use tokio::sync::broadcast;

use crate::error::AuctioneerError;

#[auto_impl::auto_impl(Arc)]
pub trait Auctioneer: Send + Sync + Clone {
    fn get_last_slot_delivered(&self) -> Option<u64>;
    fn check_and_set_last_slot_and_hash_delivered(
        &self,
        slot: u64,
        hash: &B256,
    ) -> Result<(), AuctioneerError>;

    fn save_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKeyBytes,
        block_hash: &B256,
        versioned_execution_payload: PayloadAndBlobs,
    );
    fn get_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKeyBytes,
        block_hash: &B256,
        fork_name: ForkName,
    ) -> Option<PayloadAndBlobs>;

    fn get_builder_info(
        &self,
        builder_pub_key: &BlsPublicKeyBytes,
    ) -> Result<BuilderInfo, AuctioneerError>;
    fn contains_api_key(&self, api_key: &HeaderValue) -> bool;
    fn validate_api_key(&self, api_key: &HeaderValue, pubkey: &BlsPublicKeyBytes) -> bool;

    fn demote_builder(&self, builder_pub_key: &BlsPublicKeyBytes) -> Result<(), AuctioneerError>;

    fn update_builder_infos(&self, builder_infos: &[BuilderInfoDocument]);

    fn seen_or_insert_block_hash(&self, block_hash: &B256) -> bool;

    fn update_trusted_proposers(&self, proposer_whitelist: Vec<ProposerInfo>);

    fn is_trusted_proposer(&self, proposer_pub_key: &BlsPublicKeyBytes) -> bool;

    fn save_payload_address(
        &self,
        block_hash: &B256,
        builder_pub_key: &BlsPublicKeyBytes,
        payload_url: Vec<u8>,
    );

    fn get_payload_url(&self, block_hash: &B256) -> Option<(BlsPublicKeyBytes, Vec<u8>)>;

    fn update_primev_proposers(&self, proposer_whitelist: &[BlsPublicKeyBytes]);

    fn is_primev_proposer(&self, proposer_pub_key: &BlsPublicKeyBytes) -> bool;

    fn kill_switch_enabled(&self) -> bool;

    fn enable_kill_switch(&self);

    fn disable_kill_switch(&self);

    fn update_current_inclusion_list(
        &self,
        inclusion_list: InclusionListWithMetadata,
        slot_coordinate: SlotCoordinate,
    );

    fn get_inclusion_list(&self) -> broadcast::Receiver<InclusionListWithKey>;

    fn update_proposer_duties(&self, duties: Vec<BuilderGetValidatorsResponseEntry>);

    fn get_proposer_duties(&self) -> Vec<BuilderGetValidatorsResponseEntry>;

    fn process_slot(&self, head_slot: u64);
}
