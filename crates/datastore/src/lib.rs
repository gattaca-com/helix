pub mod auctioneer;
pub mod error;
pub mod local;
pub mod types;

use std::sync::Arc;

pub use auctioneer::*;
use helix_common::bid_sorter::BidSorterMessage;
use helix_database::{postgres::postgres_db_service::PostgresDatabaseService, DatabaseService};
use local::local_cache::LocalCache;

pub async fn start_auctioneer(
    sorter_tx: crossbeam_channel::Sender<BidSorterMessage>,
    db: &PostgresDatabaseService,
) -> eyre::Result<Arc<LocalCache>> {
    let builder_infos = db.get_all_builder_infos().await.expect("failed to load builder infos");

    let auctioneer = Arc::new(LocalCache::new(builder_infos, sorter_tx).await);

    Ok(auctioneer)
}
