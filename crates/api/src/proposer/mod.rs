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
use helix_common::{
    chain_info::ChainInfo, metadata_provider::MetadataProvider, task, RelayConfig,
    ValidatorPreferences,
};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use helix_housekeeper::CurrentSlotInfo;
use helix_types::BlsPublicKey;
use hyper::StatusCode;
use tokio::sync::mpsc::{Receiver, Sender};
pub use types::*;

use crate::{
    gossiper::{traits::GossipClientTrait, types::GossipedMessage},
    proposer::{
        unblind_beacon_block, GetHeaderParams, PreferencesHeader, GET_HEADER_REQUEST_CUTOFF_MS,
    },
};

#[derive(Clone)]
pub struct ProposerApi<A, DB, G, MP>
where
    A: Auctioneer,
    DB: DatabaseService,
    G: GossipClientTrait + 'static,
    MP: MetadataProvider,
{
    pub auctioneer: Arc<A>,
    pub db: Arc<DB>,
    pub gossiper: Arc<G>,
    pub broadcasters: Vec<Arc<BlockBroadcaster>>,
    pub multi_beacon_client: Arc<MultiBeaconClient>,
    pub metadata_provider: Arc<MP>,

    /// Information about the current head slot and next proposer duty
    pub curr_slot_info: CurrentSlotInfo,
    pub chain_info: Arc<ChainInfo>,
    pub validator_preferences: Arc<ValidatorPreferences>,
    pub relay_config: RelayConfig,
    /// Channel on which to send v3 payload fetch requests.
    pub v3_payload_request: Sender<(B256, BlsPublicKey, Vec<u8>)>,
}

impl<A, DB, G, MP> ProposerApi<A, DB, G, MP>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
    G: GossipClientTrait + 'static,
    MP: MetadataProvider + 'static,
{
    pub fn new(
        auctioneer: Arc<A>,
        db: Arc<DB>,
        gossiper: Arc<G>,
        metadata_provider: Arc<MP>,
        broadcasters: Vec<Arc<BlockBroadcaster>>,
        multi_beacon_client: Arc<MultiBeaconClient>,
        chain_info: Arc<ChainInfo>,
        validator_preferences: Arc<ValidatorPreferences>,
        gossip_receiver: Receiver<GossipedMessage>,
        relay_config: RelayConfig,
        v3_payload_request: Sender<(B256, BlsPublicKey, Vec<u8>)>,
        curr_slot_info: CurrentSlotInfo,
    ) -> Self {
        let api = Self {
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
        };

        // Spin up gossip processing task
        let api_clone = api.clone();
        task::spawn(file!(), line!(), async move {
            api_clone.process_gossiped_info(gossip_receiver).await;
        });

        api
    }
}

/// Implements this API: <https://ethereum.github.io/builder-specs/#/Builder/status>
pub async fn status() -> impl IntoResponse {
    StatusCode::OK
}
