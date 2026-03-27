pub mod beacon_client;
pub mod error;
pub mod multi_beacon_client;
pub mod types;

use std::sync::Arc;

pub use beacon_client::BeaconClient;
pub use error::{ApiError, BeaconClientError};
pub use multi_beacon_client::MultiBeaconClient;
use tracing::error;

use crate::RelayConfig;

pub fn create_beacon_client(config: &RelayConfig) -> Arc<MultiBeaconClient> {
    let beacon_clients =
        config.beacon_clients.iter().map(|cfg| Arc::new(BeaconClient::new(cfg.clone()))).collect();
    Arc::new(MultiBeaconClient::new(beacon_clients))
}

/// Fetches chain info, retrying every 5s for up to 1 minute. Panics if unavailable.
pub async fn load_chain_info(beacon_client: &MultiBeaconClient) -> crate::chain_info::ChainInfo {
    let mut retry = 0u32;
    loop {
        match beacon_client.get_chain_info().await {
            Ok(info) => return info,
            Err(err) => error!(?err, retry, "failed fetching chain info, retrying.."),
        }
        retry += 1;
        if retry >= 12 {
            panic!("failed fetching chain info for 1 minute, is any beacon available?");
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}
