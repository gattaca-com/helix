pub mod beacon_client;
pub mod error;
pub mod multi_beacon_client;
pub mod types;

use std::sync::Arc;

use beacon_client::BeaconClient;
pub use helix_common::*;
use multi_beacon_client::MultiBeaconClient;

pub fn start_beacon_client(config: &RelayConfig) -> Arc<MultiBeaconClient> {
    let mut beacon_clients = vec![];
    for cfg in &config.beacon_clients {
        beacon_clients.push(Arc::new(BeaconClient::new(cfg.clone())));
    }

    let beacon_client = MultiBeaconClient::new(beacon_clients);
    tokio::spawn(beacon_client.clone().start_sync_monitor());

    Arc::new(beacon_client)
}
