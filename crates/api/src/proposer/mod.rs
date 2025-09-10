// #[cfg(test)]
// pub mod tests;

mod block_merging;
mod error;
mod get_header;
mod get_payload;
mod register;
mod types;

use std::sync::{atomic::Ordering, Arc};

use alloy_primitives::B256;
use axum::{response::IntoResponse, Extension};
pub use block_merging::MergingPoolMessage;
use helix_beacon::{multi_beacon_client::MultiBeaconClient, BlockBroadcaster};
use helix_common::{
    alerts::AlertManager, bid_sorter::BestGetHeader, chain_info::ChainInfo,
    signing::RelaySigningContext, RelayConfig, ValidatorPreferences,
};
use helix_housekeeper::CurrentSlotInfo;
use helix_types::BlsPublicKeyBytes;
use hyper::StatusCode;
use tokio::sync::mpsc::Sender;
pub use types::*;

use crate::{
    builder::multi_simulator::MultiSimulator, gossiper::grpc_gossiper::GrpcGossiperClientManager,
    proposer::block_merging::BestMergedBlock, router::Terminating, Api,
};

#[derive(Clone)]
pub struct ProposerApi<A: Api> {
    pub auctioneer: Arc<A::Auctioneer>,
    pub db: Arc<A::DatabaseService>,
    pub simulator: MultiSimulator<A::Auctioneer, A::DatabaseService>,
    pub gossiper: Arc<GrpcGossiperClientManager>,
    pub broadcasters: Vec<Arc<BlockBroadcaster>>,
    pub multi_beacon_client: Arc<MultiBeaconClient>,
    pub metadata_provider: Arc<A::MetadataProvider>,
    pub signing_context: Arc<RelaySigningContext>,

    /// Information about the current head slot and next proposer duty
    pub curr_slot_info: CurrentSlotInfo,
    pub chain_info: Arc<ChainInfo>,
    pub validator_preferences: Arc<ValidatorPreferences>,
    pub relay_config: RelayConfig,
    /// Channel on which to send v3 payload fetch requests.
    pub v3_payload_request: Sender<(u64, B256, BlsPublicKeyBytes, Vec<u8>)>,

    /// Set in the sorter loop
    pub shared_best_header: BestGetHeader,

    /// Set in the block merging process
    pub shared_best_merged: BestMergedBlock,

    pub alert_manager: AlertManager,
}

impl<A: Api> ProposerApi<A> {
    pub fn new(
        auctioneer: Arc<A::Auctioneer>,
        db: Arc<A::DatabaseService>,
        simulator: MultiSimulator<A::Auctioneer, A::DatabaseService>,
        gossiper: Arc<GrpcGossiperClientManager>,
        metadata_provider: Arc<A::MetadataProvider>,
        signing_context: Arc<RelaySigningContext>,
        broadcasters: Vec<Arc<BlockBroadcaster>>,
        multi_beacon_client: Arc<MultiBeaconClient>,
        chain_info: Arc<ChainInfo>,
        validator_preferences: Arc<ValidatorPreferences>,
        relay_config: RelayConfig,
        v3_payload_request: Sender<(u64, B256, BlsPublicKeyBytes, Vec<u8>)>,
        curr_slot_info: CurrentSlotInfo,
        shared_best_header: BestGetHeader,
    ) -> Self {
        Self {
            auctioneer,
            db,
            gossiper,
            broadcasters,
            simulator,
            signing_context,
            multi_beacon_client,
            chain_info,
            metadata_provider,
            validator_preferences,
            relay_config: relay_config.clone(),
            v3_payload_request,
            curr_slot_info,
            shared_best_header,
            shared_best_merged: BestMergedBlock::new(),
            alert_manager: AlertManager::from_relay_config(&relay_config),
        }
    }
}

/// Implements this API: <https://ethereum.github.io/builder-specs/#/Builder/status>
pub async fn status(
    Extension(Terminating(terminating)): Extension<Terminating>,
) -> impl IntoResponse {
    if terminating.load(Ordering::Relaxed) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}
