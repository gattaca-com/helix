pub mod error;
pub mod mock_database_service;
pub mod postgres;
pub mod traits;
pub mod types;

use std::sync::Arc;

use helix_common::RelayConfig;
use postgres::postgres_db_service::PostgresDatabaseService;
pub use traits::*;
pub use types::*;

pub async fn start_db_service(config: &RelayConfig) -> eyre::Result<Arc<PostgresDatabaseService>> {
    let mut postgres_db = PostgresDatabaseService::from_relay_config(config).await;
    postgres_db.init_forever().await;

    postgres_db.init_region(&config.postgres).await;
    postgres_db
        .store_builders_info(&config.builders)
        .await
        .expect("failed to store builders info from config");

    postgres_db.load_known_validators().await;
    //postgres_db.load_validator_registrations().await;
    postgres_db.start_registration_processor().await;
    postgres_db.start_block_submission_processor().await;
    postgres_db.start_header_submission_processor().await;

    Ok(Arc::new(postgres_db))
}
