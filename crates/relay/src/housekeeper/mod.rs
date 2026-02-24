pub mod chain_event_updater;
pub mod error;
#[allow(clippy::module_inception)]
pub mod housekeeper;
// #[cfg(test)]
// pub mod housekeeper_tests;
pub mod primev_service;

mod inclusion_list;
mod slot_info;

use std::sync::Arc;

pub use chain_event_updater::{ChainEventUpdater, PayloadAttributesUpdate, SlotUpdate};
use helix_common::{RelayConfig, chain_info::ChainInfo, local_cache::LocalCache};
pub use housekeeper::Housekeeper;
pub use primev_service::EthereumPrimevService;
pub use slot_info::CurrentSlotInfo;
use tokio::sync::broadcast;

use crate::{
    DbHandle, auctioneer::Event, beacon::multi_beacon_client::MultiBeaconClient,
    network::RelayNetworkManager,
};

const HEAD_EVENT_CHANNEL_SIZE: usize = 100;
const PAYLOAD_ATTRIBUTE_CHANNEL_SIZE: usize = 300;

/// Start housekeeper and chain updater
pub async fn start_housekeeper(
    db: DbHandle,
    auctioneer: Arc<LocalCache>,
    config: &RelayConfig,
    beacon_client: Arc<MultiBeaconClient>,
    chain_info: Arc<ChainInfo>,
    event_tx: crossbeam_channel::Sender<Event>,
    relay_network_api: Arc<RelayNetworkManager>,
) -> eyre::Result<CurrentSlotInfo> {
    let (head_event_sender, head_event_receiver) = broadcast::channel(HEAD_EVENT_CHANNEL_SIZE);
    beacon_client.subscribe_to_head_events(head_event_sender).await;

    let (payload_attribute_sender, payload_attribute_receiver) =
        broadcast::channel(PAYLOAD_ATTRIBUTE_CHANNEL_SIZE);
    beacon_client.subscribe_to_payload_attributes_events(payload_attribute_sender).await;

    if config.is_submission_instance {
        let housekeeper = Housekeeper::new(
            db.clone(),
            beacon_client,
            auctioneer.clone(),
            config,
            chain_info.clone(),
            event_tx.clone(),
            relay_network_api,
        );
        housekeeper.start(head_event_receiver.resubscribe()).await?;
    }

    let curr_slot_info = CurrentSlotInfo::new();
    let chain_updater =
        ChainEventUpdater::new(auctioneer, chain_info, curr_slot_info.clone(), event_tx);
    tokio::spawn(chain_updater.start(head_event_receiver, payload_attribute_receiver));

    Ok(curr_slot_info)
}
