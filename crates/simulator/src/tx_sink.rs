use std::time::Duration;

use helix_common::config::ClickhouseConfig;
use reth_tasks::TaskExecutor;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info, warn};

const TABLE: &str = "bid_submission_payloads";
const ENV_CLICKHOUSE_PASSWORD: &str = "CLICKHOUSE_PASSWORD";

#[derive(clickhouse::Row, serde::Serialize)]
pub struct TxSimRow {
    pub slot: u64,
    pub block_hash: [u8; 32],
    pub tx_hash: [u8; 32],
    pub to_address: [u8; 20],
    pub index: u32,
    pub builder_payment: [u8; 32],
    pub timestamp: i64,
    #[serde(serialize_with = "serialize_48")]
    pub builder_pubkey: [u8; 48],
}

fn serialize_48<S: serde::Serializer>(v: &[u8; 48], s: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeTuple;
    let mut tup = s.serialize_tuple(48)?;
    for byte in v {
        tup.serialize_element(byte)?;
    }
    tup.end()
}

pub struct TxSimSink {
    tx: UnboundedSender<Vec<TxSimRow>>,
}

impl TxSimSink {
    pub fn new(config: &ClickhouseConfig, task_executor: &TaskExecutor) -> Self {
        let password = std::env::var(ENV_CLICKHOUSE_PASSWORD).unwrap_or_default();
        let client = clickhouse::Client::default()
            .with_url(&config.url)
            .with_database(&config.database)
            .with_user(&config.user)
            .with_password(password);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<TxSimRow>>();

        task_executor.spawn(Box::pin(async move {
            while let Some(rows) = rx.recv().await {
                match insert_rows(&client, rows).await {
                    Ok(n) => info!(n, "inserted tx sim rows to {TABLE}"),
                    Err(e) => error!(?e, "clickhouse tx sim insert failed"),
                }
            }
            warn!("tx sim sink channel closed");
        }));

        Self { tx }
    }

    /// Enqueue rows for async insertion. Safe to call from a blocking context.
    pub fn send(&self, rows: Vec<TxSimRow>) {
        if let Err(e) = self.tx.send(rows) {
            error!(n = e.0.len(), "tx sim sink channel closed, dropped rows");
        }
    }
}

async fn insert_rows(
    client: &clickhouse::Client,
    rows: Vec<TxSimRow>,
) -> Result<usize, clickhouse::error::Error> {
    let mut insert = client
        .insert::<TxSimRow>(TABLE)
        .await?
        .with_timeouts(Some(Duration::from_secs(10)), Some(Duration::from_secs(10)));
    let len = rows.len();
    for row in rows {
        insert.write(&row).await?;
    }
    insert.end().await?;
    Ok(len)
}
