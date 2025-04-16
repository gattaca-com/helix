pub mod chain_event_updater;
pub mod error;
pub mod housekeeper;
#[cfg(test)]
pub mod housekeeper_tests;
pub mod primev_service;

use std::{sync::Arc, time::Duration};

pub use chain_event_updater::{
    ChainEventUpdater, ChainUpdate, PayloadAttributesUpdate, SlotUpdate,
};
use helix_beacon::multi_beacon_client::MultiBeaconClient;
use helix_common::{chain_info::ChainInfo, RelayConfig};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use helix_datastore::redis::redis_cache::RedisCache;
pub use housekeeper::Housekeeper;
pub use primev_service::EthereumPrimevService;
use tokio::sync::broadcast;

const HEAD_EVENT_CHANNEL_SIZE: usize = 100;
const PAYLOAD_ATTRIBUTE_CHANNEL_SIZE: usize = 300;
const CHAIN_UPDATE_CHANNEL_SIZE: usize = 200;

/// Start housekeeper and chain updater
pub async fn start_housekeeper(
    db: Arc<PostgresDatabaseService>,
    auctioneer: Arc<RedisCache>,
    config: Arc<RelayConfig>,
    beacon_client: Arc<MultiBeaconClient>,
    chain_info: Arc<ChainInfo>,
) -> eyre::Result<broadcast::Receiver<ChainUpdate>> {
    let (head_event_sender, head_event_receiver) = broadcast::channel(HEAD_EVENT_CHANNEL_SIZE);
    beacon_client.subscribe_to_head_events(head_event_sender).await;

    let (payload_attribute_sender, payload_attribute_receiver) =
        broadcast::channel(PAYLOAD_ATTRIBUTE_CHANNEL_SIZE);
    beacon_client.subscribe_to_payload_attributes_events(payload_attribute_sender).await;

    let housekeeper =
        Housekeeper::new(db.clone(), beacon_client, auctioneer.clone(), config, chain_info.clone());

    let mut housekeeper_head_events = head_event_receiver.resubscribe();
    tokio::spawn(async move {
        loop {
            if let Err(err) = housekeeper.start(&mut housekeeper_head_events).await {
                tracing::error!("Housekeeper error: {}", err);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    });

    let (chain_update_tx, chain_update_rx) = broadcast::channel(CHAIN_UPDATE_CHANNEL_SIZE);
    let chain_updater = ChainEventUpdater::new(db, auctioneer, chain_update_tx, chain_info);
    tokio::spawn(chain_updater.start(head_event_receiver, payload_attribute_receiver));

    Ok(chain_update_rx)
}
