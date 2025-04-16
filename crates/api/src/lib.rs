#![allow(clippy::too_many_arguments)]

use std::sync::Arc;

use helix_beacon::multi_beacon_client::MultiBeaconClient;
use helix_common::{chain_info::ChainInfo, signing::RelaySigningContext, RelayConfig};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use helix_datastore::redis::redis_cache::RedisCache;
use helix_housekeeper::ChainUpdate;
use service::ApiService;
use tokio::sync::broadcast;

pub mod builder;
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

pub fn start_api_service(
    config: RelayConfig,
    db: Arc<PostgresDatabaseService>,
    auctioneer: Arc<RedisCache>,
    chain_update_rx: broadcast::Receiver<ChainUpdate>,
    chain_info: Arc<ChainInfo>,
    relay_signing_context: Arc<RelaySigningContext>,
    multi_beacon_client: Arc<MultiBeaconClient>,
    metadata_provider: Arc<dyn helix_common::metadata_provider::MetadataProvider>,
) {
    tokio::spawn(ApiService::run(
        config.clone(),
        db,
        auctioneer,
        chain_update_rx,
        chain_info,
        relay_signing_context,
        multi_beacon_client,
        metadata_provider,
    ));
}
