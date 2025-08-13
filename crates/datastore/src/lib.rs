pub mod auctioneer;
pub mod error;
pub mod redis;
pub mod types;

use std::sync::Arc;

pub use auctioneer::*;
use helix_common::{bid_sorter::BidSorterMessage, RelayConfig};
use helix_database::{postgres::postgres_db_service::PostgresDatabaseService, DatabaseService};
use redis::redis_cache::RedisCache;
use tracing::error;

pub async fn start_auctioneer(
    config: &RelayConfig,
    sorter_tx: crossbeam_channel::Sender<BidSorterMessage>,
    db: &PostgresDatabaseService,
) -> eyre::Result<Arc<RedisCache>> {
    let builder_infos = db.get_all_builder_infos().await.expect("failed to load builder infos");

    let auctioneer =
        Arc::new(RedisCache::new(&config.redis.url, builder_infos, sorter_tx).await.unwrap());

    let auctioneer_clone = auctioneer.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = auctioneer_clone.start_inclusion_list_listener().await {
                error!("Inclusion list listener error: {}", err);
            }
        }
    });

    let auctioneer_clone = auctioneer.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = auctioneer_clone.start_builder_info_listener().await {
                error!("Builder info listener error: {}", err);
            }
        }
    });

    let auctioneer_clone = auctioneer.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = auctioneer_clone.start_trusted_proposers_listener().await {
                error!("Proposer whitelist listener error: {}", err);
            }
        }
    });

    let auctioneer_clone = auctioneer.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = auctioneer_clone.start_proposer_whitelist_deleted_listener().await {
                error!("Proposer whitelist deleted listener error: {}", err);
            }
        }
    });

    let auctioneer_clone = auctioneer.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = auctioneer_clone.start_payload_address_listener().await {
                error!("Payload address listener error: {}", err);
            }
        }
    });

    let auctioneer_clone = auctioneer.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = auctioneer_clone.start_builder_last_bid_received_at_listener().await {
                error!("Builder last bid received listener error: {}", err);
            }
        }
    });

    let auctioneer_clone = auctioneer.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = auctioneer_clone.start_head_event_listener().await {
                error!("Head event listener error: {}", err);
            }
        }
    });

    let auctioneer_clone = auctioneer.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = auctioneer_clone.start_last_slot_delivered_listener().await {
                error!("Last slot delivered listener error: {}", err);
            }
        }
    });

    let auctioneer_clone = auctioneer.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = auctioneer_clone.start_last_hash_delivered_listener().await {
                error!("Last hash delivered listener error: {}", err);
            }
        }
    });

    let auctioneer_clone = auctioneer.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = auctioneer_clone.start_payload_attributes_listener().await {
                error!("Payload attributes listener error: {}", err);
            }
        }
    });

    let auctioneer_clone = auctioneer.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = auctioneer_clone.start_seen_block_hashes_listener().await {
                error!("Seen block hashes listener error: {}", err);
            }
        }
    });

    Ok(auctioneer)
}
