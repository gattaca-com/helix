use std::sync::{Arc, atomic::AtomicBool};

use helix_common::{RelayConfig, load_config, local_cache::LocalCache};
use helix_database::{DbRequest, PendingBlockSubmissionValue, start_db_service};
use helix_website::WebsiteService;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    let config: RelayConfig = load_config();

    let known_validators_loaded = Arc::new(AtomicBool::new(false));
    let local_cache = Arc::new(LocalCache::new());
    let (_, db_req_rx) = crossbeam_channel::bounded::<DbRequest>(0);
    let (_, db_batch_rx) = crossbeam_channel::bounded::<PendingBlockSubmissionValue>(0);
    let db =
        start_db_service(&config, known_validators_loaded, db_req_rx, db_batch_rx, local_cache)
            .await?;

    WebsiteService::run_loop(config, db).await;

    Ok(())
}
