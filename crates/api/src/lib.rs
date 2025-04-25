#![allow(clippy::too_many_arguments)]

use std::sync::Arc;

use helix_beacon::multi_beacon_client::MultiBeaconClient;
use helix_common::{
    chain_info::ChainInfo, metadata_provider::MetadataProvider, signing::RelaySigningContext,
    RelayConfig,
};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use helix_housekeeper::CurrentSlotInfo;
use service::ApiService;

pub mod builder;
pub mod constants;
pub mod gossiper;
pub mod integration_tests;
pub mod middleware;
pub mod proposer;
pub mod relay_data;
pub mod router;
pub mod service;

#[cfg(test)]
pub mod test_utils;

mod grpc {
    include!(concat!(env!("OUT_DIR"), "/gossip.rs"));
}

pub fn start_api_service<A: Api>(
    config: RelayConfig,
    db: Arc<A::DatabaseService>,
    auctioneer: Arc<A::Auctioneer>,
    chain_info: Arc<ChainInfo>,
    relay_signing_context: Arc<RelaySigningContext>,
    multi_beacon_client: Arc<MultiBeaconClient>,
    metadata_provider: Arc<A::MetadataProvider>,
    current_slot_info: CurrentSlotInfo,
) {
    tokio::spawn(ApiService::run::<A>(
        config.clone(),
        db,
        auctioneer,
        current_slot_info,
        chain_info,
        relay_signing_context,
        multi_beacon_client,
        metadata_provider,
    ));
}

pub trait Api: Clone + Send + Sync + 'static {
    type Auctioneer: Auctioneer;
    type DatabaseService: DatabaseService;
    type MetadataProvider: MetadataProvider;
}
