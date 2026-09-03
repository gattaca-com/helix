//! Postgres-backed tests, `#[ignore]`d so `just test` and CI skip them; run with `just
//! test-integration`.

use alloy_primitives::B256;
use deadpool_postgres::{Config, ManagerConfig, Pool, RecyclingMethod};
use helix_common::PostgresConfig;
use helix_database::PostgresDatabaseService;
use helix_types::MergedBlock;
use rand::{Rng, rng};
use tokio_postgres::NoTls;

const REGION: i16 = 1;

fn test_config() -> Config {
    let mut cfg = Config::new();
    cfg.host = Some("localhost".to_string());
    cfg.port = Some(5432);
    cfg.dbname = Some("postgres".to_string());
    cfg.user = Some("postgres".to_string());
    cfg.password = Some("password".to_string());
    cfg.manager = Some(ManagerConfig { recycling_method: RecyclingMethod::Fast });
    cfg
}

fn test_postgres_config() -> PostgresConfig {
    PostgresConfig {
        hostname: "localhost".to_string(),
        port: 5432,
        db_name: "postgres".to_string(),
        user: "postgres".to_string(),
        region: REGION,
        region_name: "LOCAL".to_string(),
        pool_size: None,
    }
}

/// `merged_blocks.region_id` references `region`, so that row must exist first.
async fn setup() -> Result<(PostgresDatabaseService, Pool), Box<dyn std::error::Error>> {
    let db = PostgresDatabaseService::new(&test_config(), REGION)?;
    db.run_migrations().await?;
    db.init_region(&test_postgres_config()).await;
    Ok((db, test_config().create_pool(None, NoTls)?))
}

#[tokio::test]
#[ignore = "needs local postgres: just local-postgres"]
async fn save_merged_blocks_records_region() -> Result<(), Box<dyn std::error::Error>> {
    let (db, pool) = setup().await?;

    let block_hash = B256::from(rng().random::<[u8; 32]>());
    db.save_merged_blocks(&[MergedBlock { slot: 1, block_hash, ..Default::default() }]).await?;

    let region_id: i16 = pool
        .get()
        .await?
        .query_one("SELECT region_id FROM merged_blocks WHERE block_hash = $1", &[
            &block_hash.as_slice()
        ])
        .await?
        .get(0);

    assert_eq!(region_id, REGION);
    Ok(())
}
