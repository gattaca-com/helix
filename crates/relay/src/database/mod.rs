pub mod error;
pub mod handle;
pub mod postgres;
pub mod types;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use helix_common::{RelayConfig, is_local_dev, local_cache};
use postgres::postgres_db_service::PostgresDatabaseService;
pub use types::*;

pub use crate::database::postgres::postgres_db_service::{DbRequest, PendingBlockSubmissionValue};

pub async fn start_db_service(
    config: &RelayConfig,
    known_validators_loaded: Arc<AtomicBool>,
    db_request_receiver: crossbeam_channel::Receiver<DbRequest>,
    db_batch_request_receiver: crossbeam_channel::Receiver<PendingBlockSubmissionValue>,
    local_cache: Arc<local_cache::LocalCache>,
) -> eyre::Result<Arc<PostgresDatabaseService>> {
    let mut postgres_db =
        PostgresDatabaseService::from_relay_config(config, local_cache.clone()).await;

    if !is_local_dev() {
        postgres_db.init_forever().await;

        postgres_db.init_region(&config.postgres).await;
        postgres_db
            .store_builders_info(&config.builders)
            .await
            .expect("failed to store builders info from config");

        // housekeeper already runs in the submission instance so no need to refresh there
        let should_refresh_cache = !config.is_submission_instance;
        tokio::spawn({
            let known_validators_loaded = known_validators_loaded.clone();
            let postgres_db = postgres_db.clone();
            let local_cache = local_cache.clone();
            async move {
                postgres_db.load_known_validators().await;
                postgres_db.load_validator_registrations().await;
                postgres_db.load_builder_infos(local_cache.clone()).await;
                postgres_db.load_trusted_proposers(local_cache.clone()).await;
                postgres_db.load_validator_pools().await;
                known_validators_loaded.store(true, Ordering::Relaxed);

                if should_refresh_cache {
                    // refresh cache ~ every epoch
                    let mut tick = tokio::time::interval(Duration::from_secs(5 * 60));
                    tick.tick().await;

                    loop {
                        tick.tick().await;
                        postgres_db.load_known_validators().await;
                        postgres_db.load_validator_registrations().await;
                        postgres_db.load_builder_infos(local_cache.clone()).await;
                        postgres_db.load_trusted_proposers(local_cache.clone()).await;
                        postgres_db.load_validator_pools().await;
                    }
                }
            }
        });
    }

    postgres_db.start_processors(db_request_receiver, db_batch_request_receiver).await;

    Ok(Arc::new(postgres_db))
}
