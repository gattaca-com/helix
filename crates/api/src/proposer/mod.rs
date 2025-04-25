// #[cfg(test)]
// pub mod tests;

mod error;
mod get_header;
mod get_payload;
mod gossip;
mod register;
mod types;

use std::sync::Arc;

use alloy_primitives::B256;
use axum::response::IntoResponse;
use helix_beacon::{multi_beacon_client::MultiBeaconClient, BlockBroadcaster};
use helix_common::{chain_info::ChainInfo, RelayConfig, ValidatorPreferences};
use helix_housekeeper::CurrentSlotInfo;
use helix_types::BlsPublicKey;
use hyper::StatusCode;
use tokio::sync::mpsc::Sender;
pub use types::*;

use crate::{gossiper::grpc_gossiper::GrpcGossiperClientManager, Api};

#[derive(Clone)]
pub struct ProposerApi<A: Api> {
    pub auctioneer: Arc<A::Auctioneer>,
    pub db: Arc<A::DatabaseService>,
    pub gossiper: Arc<GrpcGossiperClientManager>,
    pub broadcasters: Vec<Arc<BlockBroadcaster>>,
    pub multi_beacon_client: Arc<MultiBeaconClient>,
    pub metadata_provider: Arc<A::MetadataProvider>,

    /// Information about the current head slot and next proposer duty
    pub curr_slot_info: CurrentSlotInfo,
    pub chain_info: Arc<ChainInfo>,
    pub validator_preferences: Arc<ValidatorPreferences>,
    pub relay_config: RelayConfig,
    /// Channel on which to send v3 payload fetch requests.
    pub v3_payload_request: Sender<(B256, BlsPublicKey, Vec<u8>)>,
}

impl<A: Api> ProposerApi<A> {
    pub fn new(
        auctioneer: Arc<A::Auctioneer>,
        db: Arc<A::DatabaseService>,
        gossiper: Arc<GrpcGossiperClientManager>,
        metadata_provider: Arc<A::MetadataProvider>,
        broadcasters: Vec<Arc<BlockBroadcaster>>,
        multi_beacon_client: Arc<MultiBeaconClient>,
        chain_info: Arc<ChainInfo>,
        validator_preferences: Arc<ValidatorPreferences>,
        relay_config: RelayConfig,
        v3_payload_request: Sender<(B256, BlsPublicKey, Vec<u8>)>,
        curr_slot_info: CurrentSlotInfo,
    ) -> Self {
        Self {
            auctioneer,
            db,
            gossiper,
            broadcasters,
            multi_beacon_client,
            chain_info,
            metadata_provider,
            validator_preferences,
            relay_config,
            v3_payload_request,
            curr_slot_info,
        }
    }
}

/// Implements this API: <https://ethereum.github.io/builder-specs/#/Builder/status>
pub async fn status() -> impl IntoResponse {
    StatusCode::OK
}
