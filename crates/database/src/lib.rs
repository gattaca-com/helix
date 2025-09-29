pub mod error;
pub mod mock_database_service;
pub mod postgres;
pub mod traits;
pub mod types;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use helix_common::{is_local_dev, RelayConfig};
use postgres::postgres_db_service::PostgresDatabaseService;
pub use traits::*;
pub use types::*;

pub async fn start_db_service(
    config: &RelayConfig,
    known_validators_loaded: Arc<AtomicBool>,
) -> eyre::Result<Arc<PostgresDatabaseService>> {
    let mut postgres_db = PostgresDatabaseService::from_relay_config(config).await;

    if !is_local_dev() {
        postgres_db.init_forever().await;

        postgres_db.init_region(&config.postgres).await;
        postgres_db
            .store_builders_info(&config.builders)
            .await
            .expect("failed to store builders info from config");

        tokio::spawn({
            let known_validators_loaded = known_validators_loaded.clone();
            let postgres_db = postgres_db.clone();
            async move {
                postgres_db.load_known_validators().await;
                known_validators_loaded.store(true, Ordering::Relaxed);
            }
        });
    }

    //postgres_db.load_validator_registrations().await;
    postgres_db.start_processors().await;

    Ok(Arc::new(postgres_db))
}
