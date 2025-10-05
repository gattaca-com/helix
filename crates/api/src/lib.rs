#![allow(clippy::too_many_arguments)]

use std::sync::{atomic::AtomicBool, Arc};

use bytes::Bytes;
use helix_beacon::multi_beacon_client::MultiBeaconClient;
use helix_common::{
    chain_info::ChainInfo, local_cache::LocalCache, metadata_provider::MetadataProvider,
    signing::RelaySigningContext, RelayConfig,
};
use helix_database::DatabaseService;
use helix_housekeeper::{chain_event_updater::SlotData, CurrentSlotInfo};
use helix_network::api::RelayNetworkApi;
use service::run_api_service;

pub mod admin_service;
pub mod auctioneer;
pub mod builder;
pub mod gossip;
pub mod gossiper;
pub mod integration_tests;
pub mod middleware;
pub mod proposer;
pub mod relay_data;
pub mod router;
pub mod service;

mod grpc {
    include!(concat!(env!("OUT_DIR"), "/gossip.rs"));
}

pub fn start_api_service<A: Api>(
    config: RelayConfig,
    db: Arc<A::DatabaseService>,
    local_cache: Arc<LocalCache>,
    chain_info: Arc<ChainInfo>,
    relay_signing_context: Arc<RelaySigningContext>,
    multi_beacon_client: Arc<MultiBeaconClient>,
    metadata_provider: Arc<A::MetadataProvider>,
    current_slot_info: CurrentSlotInfo,
    known_validators_loaded: Arc<AtomicBool>,
    terminating: Arc<AtomicBool>,
    top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
    slot_data_rx: crossbeam_channel::Receiver<SlotData>,
    relay_network_api: RelayNetworkApi,
) {
    tokio::spawn(run_api_service::<A>(
        config.clone(),
        db,
        local_cache,
        current_slot_info,
        chain_info,
        relay_signing_context,
        multi_beacon_client,
        metadata_provider,
        known_validators_loaded,
        terminating,
        top_bid_tx,
        slot_data_rx,
        relay_network_api,
    ));
}

pub fn start_admin_service(auctioneer: Arc<LocalCache>, config: &RelayConfig) {
    tokio::spawn(admin_service::run_admin_service(auctioneer, config.clone()));
}

pub trait Api: Clone + Send + Sync + 'static {
    type DatabaseService: DatabaseService;
    type MetadataProvider: MetadataProvider;
}

/// Timeout in milliseconds from when the call started
pub const HEADER_TIMEOUT_MS: &str = "x-timeout-ms";
pub const HEADER_API_KEY: &str = "x-api-key";
pub const HEADER_SEQUENCE: &str = "x-sequence";
pub const HEADER_HYDRATE: &str = "x-hydrate";
pub const HEADER_IS_MERGEABLE: &str = "x-mergeable";
