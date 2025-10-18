mod block_merging;
mod error;
mod get_header;
pub(crate) mod get_payload;
mod register;
mod types;

use std::sync::{Arc, atomic::Ordering};

use axum::{Extension, response::IntoResponse};
pub use block_merging::MergingPoolMessage;
pub use error::*;
use helix_common::{
    RelayConfig, ValidatorPreferences, alerts::AlertManager, chain_info::ChainInfo,
    local_cache::LocalCache, signing::RelaySigningContext,
};
use hyper::StatusCode;
pub use types::*;

use crate::{
    api::{Api, proposer::block_merging::BestMergedBlock, router::Terminating},
    auctioneer::{AuctioneerHandle, RegWorkerHandle},
    beacon::multi_beacon_client::MultiBeaconClient,
    database::postgres::postgres_db_service::PostgresDatabaseService,
    gossip::GrpcGossiperClientManager,
    housekeeper::CurrentSlotInfo,
};

#[derive(Clone)]
pub struct ProposerApi<A: Api> {
    pub local_cache: Arc<LocalCache>,
    pub db: Arc<PostgresDatabaseService>,
    pub gossiper: Arc<GrpcGossiperClientManager>,
    pub multi_beacon_client: Arc<MultiBeaconClient>,
    pub api_provider: Arc<A::ApiProvider>,
    pub signing_context: Arc<RelaySigningContext>,
    /// Information about the current head slot and next proposer duty
    pub curr_slot_info: CurrentSlotInfo,
    pub chain_info: Arc<ChainInfo>,
    pub validator_preferences: Arc<ValidatorPreferences>,
    pub relay_config: RelayConfig,
    /// Set in the block merging process
    pub shared_best_merged: BestMergedBlock,
    pub alert_manager: AlertManager,
    pub auctioneer_handle: AuctioneerHandle,
    pub reg_handle: RegWorkerHandle,
}

impl<A: Api> ProposerApi<A> {
    pub fn new(
        local_cache: Arc<LocalCache>,
        db: Arc<PostgresDatabaseService>,
        gossiper: Arc<GrpcGossiperClientManager>,
        api_provider: Arc<A::ApiProvider>,
        signing_context: Arc<RelaySigningContext>,
        multi_beacon_client: Arc<MultiBeaconClient>,
        chain_info: Arc<ChainInfo>,
        validator_preferences: Arc<ValidatorPreferences>,
        relay_config: RelayConfig,
        curr_slot_info: CurrentSlotInfo,
        auctioneer_handle: AuctioneerHandle,
        reg_handle: RegWorkerHandle,
    ) -> Self {
        Self {
            local_cache,
            db,
            gossiper,
            signing_context,
            multi_beacon_client,
            chain_info,
            api_provider,
            validator_preferences,
            relay_config: relay_config.clone(),
            curr_slot_info,
            shared_best_merged: BestMergedBlock::new(),
            alert_manager: AlertManager::from_relay_config(&relay_config),
            auctioneer_handle,
            reg_handle,
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
