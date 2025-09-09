pub mod auctioneer;
pub mod error;
pub mod local;

use std::sync::Arc;

pub use auctioneer::*;
use helix_common::bid_sorter::BidSorterMessage;
use helix_database::{postgres::postgres_db_service::PostgresDatabaseService, DatabaseService};
use local::local_cache::LocalCache;

pub async fn start_auctioneer(
    sorter_tx: crossbeam_channel::Sender<BidSorterMessage>,
    db: Arc<PostgresDatabaseService>,
) -> eyre::Result<Arc<LocalCache>> {
    let auctioneer = Arc::new(LocalCache::new(sorter_tx).await);
    let auctioneer_clone = auctioneer.clone();
    tokio::spawn(async move {
        let builder_infos = db.get_all_builder_infos().await.expect("failed to load builder infos");
        auctioneer_clone.update_builder_infos(&builder_infos, true);
    });

    Ok(auctioneer)
}
