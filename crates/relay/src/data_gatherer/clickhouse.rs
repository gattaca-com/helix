use std::{collections::VecDeque, time::Duration};

use alloy_primitives::B256;
use flux_clickhouse::{ClickHouse, Error};
use flux_network::tcp::{TcpEvent, TcpNetworkCore};
use flux_utils::ArrayStr;
use helix_common::{config::ClickhouseConfig, expect_env_var};
use helix_types::BlsPublicKeyBytes;
use rustc_hash::FxHashMap;
use tracing::{error, info};

const TABLE: &str = "relay_bid_submission_data";
const ENV_CLICKHOUSE_PASSWORD: &str = "CLICKHOUSE_PASSWORD";
/// A slot's snapshot is one insert; the spare covers a slow predecessor.
const CONNECTIONS: usize = 2;
/// Rows per insert: a row encodes to a few hundred bytes, so a batch stays
/// far under the HTTP client's 1 MiB body cap whatever the slot holds.
const BATCH_ROWS: usize = 1000;
/// Unsent batches retained across a stall. Past this the oldest batch drops
/// (counted and logged): telemetry yields to memory.
const MAX_UNSENT_BATCHES: usize = 64;
/// Bytes the client may hold across queued and in-flight inserts.
const MAX_QUEUED_BYTES: usize = 64 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

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

#[derive(Clone, serde::Serialize)]
pub struct BlockInfoRow {
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
    client: ClickHouse,
    instance_id: ArrayStr<64>,
    map: FxHashMap<B256, BlockInfo>,
    in_flight: usize,
    /// Batches the client refused (full queue); retried on every drive.
    /// Refused work was never sent, so retrying duplicates nothing.
    unsent: VecDeque<Vec<BlockInfoRow>>,
    dropped_rows: u64,
}

impl ClickhouseData {
    pub fn new(config: &ClickhouseConfig, instance_id: String, net: &mut TcpNetworkCore) -> Self {
        let password = expect_env_var(ENV_CLICKHOUSE_PASSWORD);
        let mut client = ClickHouse::new(config.addr, CONNECTIONS)
            .with_credentials(&config.user, &password)
            .with_database(&config.database)
            .with_request_timeout(REQUEST_TIMEOUT)
            .with_max_queued_bytes(MAX_QUEUED_BYTES);
        client.connect(net);
        Self {
            client,
            instance_id: ArrayStr::from_str_truncate(&instance_id),
            map: FxHashMap::with_capacity_and_hasher(5000, Default::default()),
            in_flight: 0,
            unsent: VecDeque::new(),
            dropped_rows: 0,
        }
    }

    pub fn insert(&mut self, hash: B256, info: BlockInfo) {
        self.map.insert(hash, info);
    }

    pub fn get_mut(&mut self, hash: &B256) -> Option<&mut BlockInfo> {
        self.map.get_mut(hash)
    }

    /// Inserts still awaiting an outcome, plus batches still awaiting a send.
    pub fn pending(&self) -> usize {
        self.in_flight + self.unsent.len()
    }

    /// Queues every row belonging to a slot before `new_slot`.
    pub fn publish_snapshot(&mut self, new_slot: u64) {
        if self.map.is_empty() {
            return;
        }

        let mut rows = self
            .map
            .extract_if(|_, v| v.slot < new_slot)
            .map(|(hash, info)| BlockInfoRow::from(self.instance_id, hash, info))
            .peekable();

        while rows.peek().is_some() {
            let batch: Vec<BlockInfoRow> = rows.by_ref().take(BATCH_ROWS).collect();
            if self.unsent.len() >= MAX_UNSENT_BATCHES {
                let dropped = self.unsent.pop_front().expect("full queue has a front");
                self.dropped_rows += dropped.len() as u64;
                error!(
                    rows = dropped.len(),
                    total_dropped = self.dropped_rows,
                    "{TABLE} insert backlog full, oldest batch dropped"
                );
            }
            self.unsent.push_back(batch);
        }
        self.push_batches();
    }

    /// Sends retained batches until the client refuses one.
    fn push_batches(&mut self) {
        while let Some(batch) = self.unsent.front() {
            match self.client.insert_rows(TABLE, batch) {
                Ok(_) => {
                    let batch = self.unsent.pop_front().expect("just peeked");
                    self.in_flight += 1;
                    info!(rows = batch.len(), "queued insert to {TABLE}");
                }
                Err(_) => break,
            }
        }
    }

    /// Returns whether the event belonged to this client.
    pub fn on_event(&mut self, event: &TcpEvent<'_>) -> bool {
        self.client.on_event(event)
    }

    /// Sends what is queued and retires finished inserts. A sent insert
    /// that times out is logged, not retried: it may have run, and the
    /// table has no key to dedupe a repeat.
    pub fn drive(&mut self, net: &mut TcpNetworkCore) -> bool {
        self.push_batches();
        let in_flight = &mut self.in_flight;
        let mut worked = false;
        self.client.drive(net, |_, result| {
            worked = true;
            *in_flight = in_flight.saturating_sub(1);
            match result {
                Ok(_) => {}
                Err(Error::Server { code, name, message }) => error!(
                    code,
                    name,
                    detail = %message,
                    "failed to insert rows to {TABLE}"
                ),
                Err(other) => error!(?other, "failed to insert rows to {TABLE}"),
            }
        });
        worked
    }
}
