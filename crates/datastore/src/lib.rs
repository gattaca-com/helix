pub mod auctioneer;
pub mod error;
pub mod redis;
pub mod types;

use std::{sync::Arc, time::Duration};

pub use auctioneer::*;
use helix_common::RelayConfig;
use helix_database::{postgres::postgres_db_service::PostgresDatabaseService, DatabaseService};
use redis::redis_cache::RedisCache;
use tokio::time::sleep;

pub async fn start_auctioneer(
    config: &RelayConfig,
    db: &PostgresDatabaseService,
) -> eyre::Result<Arc<RedisCache>> {
    let builder_infos = db.get_all_builder_infos().await.expect("failed to load builder infos");

    let auctioneer = Arc::new(RedisCache::new(&config.redis.url, builder_infos).await.unwrap());

    let auctioneer_clone = auctioneer.clone();
    tokio::spawn(async move {
        loop {
            if let Err(err) = auctioneer_clone.start_best_bid_listener().await {
                tracing::error!("Bid listener error: {}", err);
                sleep(Duration::from_secs(5)).await;
            }
        }
    });

    Ok(auctioneer)
}
