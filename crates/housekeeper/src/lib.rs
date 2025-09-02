pub mod chain_event_updater;
pub mod error;
pub mod housekeeper;
#[cfg(test)]
pub mod housekeeper_tests;
pub mod primev_service;

mod inclusion_list;
mod slot_info;

use std::sync::Arc;

pub use chain_event_updater::{ChainEventUpdater, PayloadAttributesUpdate, SlotUpdate};
use helix_beacon::multi_beacon_client::MultiBeaconClient;
use helix_common::{bid_sorter::BidSorterMessage, chain_info::ChainInfo, RelayConfig};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use helix_datastore::local::local_cache::LocalCache;
pub use housekeeper::Housekeeper;
pub use primev_service::EthereumPrimevService;
pub use slot_info::CurrentSlotInfo;
use tokio::sync::broadcast;

const HEAD_EVENT_CHANNEL_SIZE: usize = 100;
const PAYLOAD_ATTRIBUTE_CHANNEL_SIZE: usize = 300;

/// Start housekeeper and chain updater
pub async fn start_housekeeper(
    db: Arc<PostgresDatabaseService>,
    auctioneer: Arc<LocalCache>,
    config: &RelayConfig,
    beacon_client: Arc<MultiBeaconClient>,
    chain_info: Arc<ChainInfo>,
    sorter_tx: crossbeam_channel::Sender<BidSorterMessage>,
) -> eyre::Result<CurrentSlotInfo> {
    let (head_event_sender, head_event_receiver) = broadcast::channel(HEAD_EVENT_CHANNEL_SIZE);
    beacon_client.subscribe_to_head_events(head_event_sender).await;

    let (payload_attribute_sender, payload_attribute_receiver) =
        broadcast::channel(PAYLOAD_ATTRIBUTE_CHANNEL_SIZE);
    beacon_client.subscribe_to_payload_attributes_events(payload_attribute_sender).await;

    if config.housekeeper {
        let housekeeper = Housekeeper::new(
            db.clone(),
            beacon_client,
            auctioneer.clone(),
            config,
            chain_info.clone(),
        );
        housekeeper.start(head_event_receiver.resubscribe()).await?;
    }

    let curr_slot_info = CurrentSlotInfo::new();
    let chain_updater =
        ChainEventUpdater::new(auctioneer, chain_info, curr_slot_info.clone(), sorter_tx);
    tokio::spawn(chain_updater.start(head_event_receiver, payload_attribute_receiver));

    Ok(curr_slot_info)
}
