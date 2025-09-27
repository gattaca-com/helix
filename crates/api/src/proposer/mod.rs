// #[cfg(test)]
// pub mod tests;

mod block_merging;
mod error;
mod get_header;
mod get_payload;
mod register;
mod types;

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use axum::{response::IntoResponse, Extension};
pub use block_merging::MergingPoolMessage;
pub use error::*;
use helix_beacon::multi_beacon_client::MultiBeaconClient;
use helix_common::{
    alerts::AlertManager, chain_info::ChainInfo, local_cache::LocalCache,
    signing::RelaySigningContext, RelayConfig, ValidatorPreferences,
};
use helix_housekeeper::CurrentSlotInfo;
use hyper::StatusCode;
use tokio::sync::mpsc::{self};
pub use types::*;

use crate::{
    builder::{
        simulator_2::{worker::WorkerJob, Event},
        BlockMergeRequest,
    },
    gossiper::grpc_gossiper::GrpcGossiperClientManager,
    proposer::block_merging::BestMergedBlock,
    router::Terminating,
    Api,
};

#[derive(Clone)]
pub struct ProposerApi<A: Api> {
    pub auctioneer: Arc<LocalCache>,
    pub db: Arc<A::DatabaseService>,
    pub gossiper: Arc<GrpcGossiperClientManager>,
    pub multi_beacon_client: Arc<MultiBeaconClient>,
    pub metadata_provider: Arc<A::MetadataProvider>,
    pub signing_context: Arc<RelaySigningContext>,

    /// Information about the current head slot and next proposer duty
    pub curr_slot_info: CurrentSlotInfo,
    pub chain_info: Arc<ChainInfo>,
    pub validator_preferences: Arc<ValidatorPreferences>,
    pub relay_config: RelayConfig,
    // /// Channel on which to send v3 payload fetch requests.
    // pub v3_payload_request: Sender<(u64, B256, BlsPublicKeyBytes, Vec<u8>)>,
    /// Set in the block merging process
    pub shared_best_merged: BestMergedBlock,
    /// Send simulation requests
    pub merge_requests_tx: mpsc::Sender<BlockMergeRequest>,
    pub alert_manager: AlertManager,
    // TODO: set this
    pub bid_slot: Arc<AtomicU64>,

    pub auctioneer_tx: crossbeam_channel::Sender<Event>,
    pub worker_tx: crossbeam_channel::Sender<WorkerJob>,
}

impl<A: Api> ProposerApi<A> {
    pub fn new(
        auctioneer: Arc<LocalCache>,
        db: Arc<A::DatabaseService>,
        gossiper: Arc<GrpcGossiperClientManager>,
        metadata_provider: Arc<A::MetadataProvider>,
        signing_context: Arc<RelaySigningContext>,
        multi_beacon_client: Arc<MultiBeaconClient>,
        chain_info: Arc<ChainInfo>,
        validator_preferences: Arc<ValidatorPreferences>,
        relay_config: RelayConfig,
        curr_slot_info: CurrentSlotInfo,
        merge_requests_tx: mpsc::Sender<BlockMergeRequest>,
        bid_slot: Arc<AtomicU64>,
        auctioneer_tx: crossbeam_channel::Sender<Event>,
        worker_tx: crossbeam_channel::Sender<WorkerJob>,
    ) -> Self {
        Self {
            auctioneer,
            db,
            gossiper,
            signing_context,
            multi_beacon_client,
            chain_info,
            metadata_provider,
            validator_preferences,
            relay_config: relay_config.clone(),
            curr_slot_info,
            shared_best_merged: BestMergedBlock::new(),
            alert_manager: AlertManager::from_relay_config(&relay_config),
            merge_requests_tx,
            bid_slot,
            auctioneer_tx,
            worker_tx,
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
