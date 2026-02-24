pub mod error;
pub mod handle;
pub mod postgres;
pub mod snapshot;
pub mod types;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use helix_common::{RelayConfig, is_local_dev, local_cache};
use postgres::postgres_db_service::PostgresDatabaseService;
use tracing::info;
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

        let snapshot_dir = config.snapshot_dir.clone();

        tokio::spawn({
            let known_validators_loaded = known_validators_loaded.clone();
            let postgres_db = postgres_db.clone();
            let local_cache = local_cache.clone();
            let is_registration_instance = config.is_registration_instance;
            let mut validator_reg_update_time = SystemTime::now();
            async move {
                load_known_validators_with_snapshot(
                    &postgres_db,
                    &local_cache,
                    snapshot_dir.as_deref(),
                )
                .await;

                load_validator_registrations_with_snapshot(
                    &postgres_db,
                    &local_cache,
                    snapshot_dir.as_deref(),
                )
                .await;

                postgres_db.load_builder_infos(local_cache.clone()).await;
                known_validators_loaded.store(true, Ordering::Relaxed);

                let mut tick = tokio::time::interval(Duration::from_secs(5 * 60));
                tick.tick().await;

                loop {
                    tick.tick().await;
                    // On submission instances cache is updated by the housekeeper
                    if is_registration_instance{
                        postgres_db.load_known_validators().await;
                    }
                    if let Some(dir) = &snapshot_dir {
                        let set = local_cache.known_validators_cache.read().clone();
                        snapshot::save_known_validators_bg(dir, &set);
                    }
                    let fetch_time = SystemTime::now();
                    postgres_db.update_validator_registrations(validator_reg_update_time).await;
                    validator_reg_update_time = fetch_time;
                    if let Some(dir) = &snapshot_dir {
                        save_validator_registrations_snapshot(&local_cache, dir);
                    }
                    postgres_db.load_builder_infos(local_cache.clone()).await;
                }
            }
        });
    }

    postgres_db.start_processors(db_request_receiver, db_batch_request_receiver).await;

    Ok(Arc::new(postgres_db))
}

async fn load_known_validators_with_snapshot(
    db: &PostgresDatabaseService,
    cache: &local_cache::LocalCache,
    snapshot_dir: Option<&std::path::Path>,
) {
    if let Some(dir) = snapshot_dir &&
        let Some(set) = snapshot::try_load_known_validators(dir).await
    {
        info!(count = set.len(), "using known_validators snapshot");
        *cache.known_validators_cache.write() = set;
        return;
    }
    // Fallback to DB
    db.load_known_validators().await;
    // Save snapshot for next startup
    if let Some(dir) = snapshot_dir {
        let set = cache.known_validators_cache.read().clone();
        snapshot::save_known_validators_bg(dir, &set);
    }
}

async fn load_validator_registrations_with_snapshot(
    db: &PostgresDatabaseService,
    cache: &local_cache::LocalCache,
    snapshot_dir: Option<&std::path::Path>,
) {
    if let Some(dir) = snapshot_dir &&
        let Some(entries) = snapshot::try_load_validator_registrations(dir).await
    {
        let count = entries.len();
        info!(count, "using validator_registrations snapshot");
        for (key, entry) in entries {
            cache.validator_registration_cache.insert(key, entry);
        }
        return;
    }
    // Fallback to DB
    db.load_validator_registrations().await;
    // Save snapshot for next startup
    if let Some(dir) = snapshot_dir {
        save_validator_registrations_snapshot(cache, dir);
    }
}

fn save_validator_registrations_snapshot(cache: &local_cache::LocalCache, dir: &std::path::Path) {
    let entries: Vec<_> =
        cache.validator_registration_cache.iter().map(|r| (*r.key(), r.value().clone())).collect();
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = snapshot::save_validator_registrations(&dir, &entries) {
            tracing::warn!("failed to save validator_registrations snapshot: {e}");
        }
    });
}
