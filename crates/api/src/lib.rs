#![allow(clippy::too_many_arguments)]

use std::sync::{atomic::AtomicBool, Arc};

use bytes::Bytes;
use helix_beacon::multi_beacon_client::MultiBeaconClient;
use helix_common::{
    bid_sorter::{BestGetHeader, BidSorterMessage, FloorBid},
    chain_info::ChainInfo,
    metadata_provider::MetadataProvider,
    signing::RelaySigningContext,
    RelayConfig,
};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use helix_housekeeper::CurrentSlotInfo;
use service::run_api_service;

pub mod admin_service;
pub mod builder;
pub mod constants;
pub mod gossip;
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
    terminating: Arc<AtomicBool>,
    sorter_tx: crossbeam_channel::Sender<BidSorterMessage>,
    top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
    shared_best_header: BestGetHeader,
    shared_floor: FloorBid,
) {
    tokio::spawn(run_api_service::<A>(
        config.clone(),
        db,
        auctioneer,
        current_slot_info,
        chain_info,
        relay_signing_context,
        multi_beacon_client,
        metadata_provider,
        terminating,
        sorter_tx,
        top_bid_tx,
        shared_best_header,
        shared_floor,
    ));
}

pub fn start_admin_service<A: Auctioneer + 'static>(auctioneer: Arc<A>, config: &RelayConfig) {
    tokio::spawn(admin_service::run_admin_service(auctioneer, config.clone()));
}

pub trait Api: Clone + Send + Sync + 'static {
    type Auctioneer: Auctioneer;
    type DatabaseService: DatabaseService;
    type MetadataProvider: MetadataProvider;
}

/// Timeout in milliseconds from when the call started
pub const HEADER_TIMEOUT_MS: &str = "x-timeout-ms";
