#![allow(clippy::future_not_send)]

use std::time::Duration;

use helix_common::{config::ClickhouseConfig, expect_env_var};
use tracing::{error, info};

const TABLE: &str = "relay_bid_submission_data";
const ENV_CLICKHOUSE_PASSWORD: &str = "CLICKHOUSE_PASSWORD";

#[derive(clickhouse::Row, serde::Serialize)]
pub struct BlockInfoRow {
    pub instance_id: String,
    pub slot: u64,
    pub is_dehydrated: bool,
    pub block_hash: String,
    pub received_ns: i64,
    pub read_body_ns: i64,
    pub decoded_ns: Option<i64>,
    pub live_ns: Option<i64>,
    pub top_bid_ns: Option<i64>,
    pub builder_pubkey: String,
}

pub struct ClickhouseData {
    client: clickhouse::Client,
}

impl ClickhouseData {
    pub fn new(config: &ClickhouseConfig) -> Self {
        let password = expect_env_var(ENV_CLICKHOUSE_PASSWORD);
        let client = clickhouse::Client::default()
            .with_url(&config.url)
            .with_database(&config.database)
            .with_user(&config.user)
            .with_password(password);
        Self { client }
    }

    pub async fn publish(&mut self, rows: impl Iterator<Item = BlockInfoRow>) {
        match self.insert_rows(rows).await {
            Ok(len) => info!("inserted {len} rows to {TABLE}"),
            Err(err) => error!(?err, "failed to insert rows to {TABLE}"),
        }
    }

    async fn insert_rows(
        &self,
        rows: impl Iterator<Item = BlockInfoRow>,
    ) -> Result<usize, clickhouse::error::Error> {
        let mut insert = self
            .client
            .insert::<BlockInfoRow>(TABLE)
            .await?
            .with_timeouts(Some(Duration::from_secs(5)), Some(Duration::from_secs(5)));
        let mut len = 0;
        for row in rows {
            insert.write(&row).await?;
            len += 1;
        }
        insert.end().await?;
        Ok(len)
    }
}
