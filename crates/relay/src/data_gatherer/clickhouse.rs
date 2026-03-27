#![allow(clippy::future_not_send)]

use std::{future::Future, time::Duration};

use alloy_primitives::B256;
use flux_utils::ArrayStr;
use helix_common::{config::ClickhouseConfig, expect_env_var};
use helix_types::BlsPublicKeyBytes;
use rustc_hash::FxHashMap;
use tracing::{error, info};

const TABLE: &str = "relay_bid_submission_data";
const ENV_CLICKHOUSE_PASSWORD: &str = "CLICKHOUSE_PASSWORD";

fn serialize_str<T: AsRef<str>, S: serde::Serializer>(v: &T, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(v.as_ref())
}

#[derive(Default)]
pub struct BlockInfo {
    pub builder_pubkey: BlsPublicKeyBytes,
    pub slot: u64,
    pub is_dehydrated: bool,
    pub received_ns: i64,
    pub read_body_ns: i64,
    pub decoded_ns: Option<i64>,
    pub live_ns: Option<i64>,
    pub top_bid_ns: Option<i64>,
}

#[derive(clickhouse::Row, serde::Serialize)]
pub struct BlockInfoRow {
    #[serde(serialize_with = "serialize_str")]
    pub instance_id: ArrayStr<64>,
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

impl BlockInfoRow {
    pub fn from(instance_id: ArrayStr<64>, block_hash: B256, info: BlockInfo) -> Self {
        BlockInfoRow {
            instance_id,
            slot: info.slot,
            block_hash: block_hash.to_string(),
            is_dehydrated: info.is_dehydrated,
            received_ns: info.received_ns,
            read_body_ns: info.read_body_ns,
            decoded_ns: info.decoded_ns,
            live_ns: info.live_ns,
            top_bid_ns: info.top_bid_ns,
            builder_pubkey: info.builder_pubkey.to_string(),
        }
    }
}

pub struct ClickhouseData {
    client: clickhouse::Client,
    instance_id: ArrayStr<64>,
    map: FxHashMap<B256, BlockInfo>,
}

impl ClickhouseData {
    pub fn new(config: &ClickhouseConfig, instance_id: String) -> Self {
        let password = expect_env_var(ENV_CLICKHOUSE_PASSWORD);
        let client = clickhouse::Client::default()
            .with_url(&config.url)
            .with_database(&config.database)
            .with_user(&config.user)
            .with_password(password);
        Self {
            client,
            instance_id: ArrayStr::from_str_truncate(&instance_id),
            map: FxHashMap::with_capacity_and_hasher(5000, Default::default()),
        }
    }

    pub fn insert(&mut self, hash: B256, info: BlockInfo) {
        self.map.insert(hash, info);
    }

    pub fn get_mut(&mut self, hash: &B256) -> Option<&mut BlockInfo> {
        self.map.get_mut(hash)
    }

    pub fn publish_snapshot(
        &mut self,
        new_slot: u64,
    ) -> Option<impl Future<Output = ()> + Send + 'static> {
        if self.map.is_empty() {
            return None;
        }

        let rows = self
            .map
            .extract_if(|_, v| v.slot < new_slot)
            .map(|(hash, info)| BlockInfoRow::from(self.instance_id, hash, info))
            .collect::<Vec<BlockInfoRow>>();

        let client = self.client.clone();
        Some(async move {
            match Self::insert_rows(&client, rows.into_iter()).await {
                Ok(len) => info!("inserted {len} rows to {TABLE}"),
                Err(err) => error!(?err, "failed to insert rows to {TABLE}"),
            }
        })
    }

    async fn insert_rows(
        client: &clickhouse::Client,
        rows: impl Iterator<Item = BlockInfoRow>,
    ) -> Result<usize, clickhouse::error::Error> {
        let mut insert = client
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
