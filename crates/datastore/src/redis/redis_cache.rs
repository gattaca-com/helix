#![allow(dependency_on_unit_never_type_fallback)] // TODO: temp fix , needs to be fixed before upading to 2024 edition

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use alloy_primitives::{bytes::Bytes, B256, U256};
use async_trait::async_trait;
use deadpool_redis::{Config, Connection, CreatePoolError, Pool, Runtime};
use futures_util::StreamExt;
use helix_beacon::types::{HeadEventData, PayloadAttributesEvent};
use helix_common::{
    api::builder_api::{
        BuilderGetValidatorsResponseEntry, InclusionListWithMetadata, TopBidUpdate,
    },
    bid_submission::{v2::header_submission::SignedHeaderSubmission, BidSubmission},
    bid_submission_to_builder_bid, header_submission_to_builder_bid,
    metrics::{RedisMetricRecord, TopBidMetrics},
    pending_block::PendingBlock,
    signing::RelaySigningContext,
    BuilderInfo, ProposerInfo,
};
use helix_database::types::BuilderInfoDocument;
use helix_types::{
    maybe_upgrade_execution_payload, BidTrace, BlsPublicKey, ForkName, PayloadAndBlobs,
    SignedBidSubmission, SignedBuilderBid,
};
use moka::sync::Cache;
use redis::{AsyncCommands, Msg, RedisResult, Script, Value};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::from_str;
use ssz::Encode;
use tokio::sync::broadcast;
use tracing::{error, info, instrument, warn};

use super::utils::{
    get_hash_from_hex, get_header_tx_root_key, get_pending_block_builder_block_hash_key,
    get_pending_block_builder_key, get_pubkey_from_hex,
};
use crate::{
    error::AuctioneerError,
    redis::{
        error::RedisCacheError,
        utils::{
            get_builder_latest_bid_time_key, get_builder_latest_bid_value_key,
            get_cache_bid_trace_key, get_cache_get_header_response_key, get_execution_payload_key,
            get_floor_bid_key, get_floor_bid_value_key, get_latest_bid_by_builder_key,
            get_latest_bid_by_builder_key_str_builder_pub_key, get_seen_block_hashes_key,
            get_top_bid_value_key,
        },
    },
    types::{
        keys::{
            BUILDER_INFO_KEY, CURRENT_INCLUSION_LIST_KEY, HOUSEKEEPER_LOCK_KEY, KILL_SWITCH,
            LAST_HASH_DELIVERED_KEY, LAST_SLOT_DELIVERED_KEY, PAYLOAD_ADDRESS_KEY,
            PRIMEV_PROPOSERS_KEY, PROPOSER_WHITELIST_KEY,
        },
        signed_builder_bid_wrapper::SignedBuilderBidWrapper,
        SaveBidAndUpdateTopBidResponse,
    },
    Auctioneer,
};

const BID_CACHE_EXPIRY_S: usize = 45;
const PENDING_BLOCK_EXPIRY_S: usize = 45;
const INCLUSION_LIST_EXPIRY_S: usize = 45;
const PAYLOAD_ADDRESS_EXPIRY_S: usize = 24;
const HOUSEKEEPER_LOCK_EXPIRY_MS: usize = 45_000;

const BEST_BIDS_CHANNEL: &str = "best_bids";
const FLOOR_BIDS_CHANNEL: &str = "floor_bids";
const CURRENT_INCLUSION_LIST_CHANNEL: &str = "inclusion_list_updates";
const SEEN_BLOCK_HASHES_CHANNEL: &str = "seen_block_hashes_updates";
const LAST_SLOT_DELIVERED_CHANNEL: &str = "last_slot_delivered_updates";
const BUILDER_LAST_BID_RECEIVED_AT_CHANNEL: &str = "builder_last_bid_received_at_updates";
const BUILDER_LAST_BID_RECEIVED_AT_DELETED_CHANNEL: &str =
    "builder_last_bid_received_at_deleted_updates";
const HEADER_TX_ROOT_CHANNEL: &str = "header_tx_root_updates";
const BUILDER_INFO_CHANNEL: &str = "builder_info_channel";

const RENEW_SCRIPT: &str = r#"
if redis.call('get', KEYS[1]) == ARGV[1] then
    return redis.call('set', KEYS[1], ARGV[1], 'PX', ARGV[2], 'XX')
end
return nil
"#;

#[derive(Clone)]
pub struct RedisCache {
    pool: Pool,
    tx: broadcast::Sender<Bytes>,
    inclusion_list: broadcast::Sender<InclusionListWithKey>,
    payload_attributes: broadcast::Sender<PayloadAttributesEvent>,
    head_event: broadcast::Sender<HeadEventData>,
    top_bid_value_cache: Cache<String, U256>,
    floor_bid_value_cache: Cache<String, U256>,
    seen_block_hashes: Cache<String, ()>,
    last_delivered_slot: Arc<AtomicU64>,
    builder_latest_payload_received_at: Cache<String, u64>,
    header_tx_root_cache: Cache<String, B256>,
    builder_info_cache: Cache<String, BuilderInfo>,
}

#[allow(dead_code)]
impl RedisCache {
    pub async fn new(
        conn_str: &str,
        builder_infos: Vec<BuilderInfoDocument>,
    ) -> Result<Self, CreatePoolError> {
        let mut cfg = Config::from_url(conn_str);
        let mut pool_config = deadpool_redis::PoolConfig::default();
        pool_config.max_size += 10; // Increase max size to accommodate listeners
        cfg.pool = Some(pool_config);
        let pool = cfg.create_pool(Some(Runtime::Tokio1))?;
        let (tx, mut tx_recv) = broadcast::channel(1000);
        let (inclusion_list, mut il_recv) = broadcast::channel(1);
        let (payload_attributes, mut pa_recv) = broadcast::channel(1000);
        let (head_event, mut he_recv) = broadcast::channel(1000);

        // ensure at least one subscriber is running
        tokio::spawn(async move { while let Ok(_message) = tx_recv.recv().await {} });
        tokio::spawn(async move { while let Ok(_message) = il_recv.recv().await {} });
        tokio::spawn(async move { while let Ok(_message) = pa_recv.recv().await {} });
        tokio::spawn(async move { while let Ok(_message) = he_recv.recv().await {} });

        let top_bid_value_cache =
            Cache::builder().time_to_idle(Duration::from_secs(45)).max_capacity(10_000).build();
        let floor_bid_value_cache =
            Cache::builder().time_to_idle(Duration::from_secs(45)).max_capacity(10_000).build();
        let seen_block_hashes =
            Cache::builder().time_to_idle(Duration::from_secs(45)).max_capacity(10_000).build();
        let builder_latest_payload_received_at =
            Cache::builder().time_to_idle(Duration::from_secs(45)).max_capacity(10_000).build();
        let header_tx_root_cache =
            Cache::builder().time_to_idle(Duration::from_secs(45)).max_capacity(10_000).build();
        let builder_info_cache =
            Cache::builder().time_to_idle(Duration::from_secs(300)).max_capacity(10_000).build();
        let last_delivered_slot = Arc::new(AtomicU64::new(0));

        let cache = Self {
            pool,
            tx,
            inclusion_list,
            payload_attributes,
            head_event,
            top_bid_value_cache,
            floor_bid_value_cache,
            seen_block_hashes,
            builder_latest_payload_received_at,
            last_delivered_slot,
            header_tx_root_cache,
            builder_info_cache,
        };

        // Load in builder info
        if let Err(err) = cache.update_builder_infos(&builder_infos).await {
            error!(err=%err, "Failed to initialise builder info")
        }

        Ok(cache)
    }

    pub async fn start_best_bid_listener(&self) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe(BEST_BIDS_CHANNEL).await?;

        let mut message_stream = pubsub.on_message();

        let mut conn = self.pool.get().await?;

        while let Some(message) = message_stream.next().await {
            let Ok((key, sig_bid)) =
                Self::process_pubsub_update::<SignedBuilderBidWrapper>(message, &mut conn).await
            else {
                continue;
            };

            let received_at = sig_bid.received_at_ms;

            let top_bid_update: TopBidUpdate = sig_bid.into();

            self.top_bid_value_cache.insert(key, top_bid_update.value);

            //ssz encode the top bid update
            let serialized = Bytes::from(top_bid_update.as_ssz_bytes());

            TopBidMetrics::top_bid_update_count();
            TopBidMetrics::received_at(received_at);
            if let Err(err) = self.tx.send(serialized) {
                error!(%err, "Failed to send top bid update");
                continue;
            }
        }

        Ok(())
    }

    pub async fn start_payload_attributes_listener(&self) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe("payload_attributes_channel").await?;

        let mut message_stream = pubsub.on_message();

        while let Some(msg) = message_stream.next().await {
            let payload: redis::RedisResult<String> = msg.get_payload();

            let Ok(json) = payload else {
                warn!("Malformed Redis payload");
                continue;
            };

            let Ok(event) = from_str::<PayloadAttributesEvent>(&json) else {
                warn!("Failed to deserialize PayloadAttributesEvent");
                continue;
            };

            if let Err(err) = self.payload_attributes.send(event) {
                error!(%err, "Failed to forward payload attributes event");
            }
        }

        Ok(())
    }

    pub async fn start_head_event_listener(&self) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe("head_event_channel").await?;

        let mut message_stream = pubsub.on_message();

        while let Some(msg) = message_stream.next().await {
            let payload: redis::RedisResult<String> = msg.get_payload();

            let Ok(json) = payload else {
                warn!("Malformed Redis payload");
                continue;
            };

            let Ok(event) = from_str::<HeadEventData>(&json) else {
                warn!("Failed to deserialize HeadEventData");
                continue;
            };

            if let Err(err) = self.head_event.send(event) {
                error!(%err, "Failed to forward head event");
            }
        }

        Ok(())
    }

    pub async fn start_inclusion_list_listener(&self) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe(CURRENT_INCLUSION_LIST_CHANNEL).await?;

        let mut message_stream = pubsub.on_message();
        let mut conn = self.pool.get().await?;

        while let Some(msg) = message_stream.next().await {
            let Ok((key, list)) = Self::process_pubsub_update(msg, &mut conn).await else {
                continue;
            };

            let list_with_key = InclusionListWithKey { key, inclusion_list: list };
            if let Err(err) = self.inclusion_list.send(list_with_key) {
                error!(%err, "Failed to send inclusion list update");
            }
        }

        Ok(())
    }

    pub async fn start_seen_block_hashes_listener(&self) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe(SEEN_BLOCK_HASHES_CHANNEL).await?;

        let mut message_stream = pubsub.on_message();

        while let Some(message) = message_stream.next().await {
            let payload = message.get_payload::<String>().ok();
            if let Some(key) = payload {
                self.seen_block_hashes.insert(key, ());
            }
        }

        Ok(())
    }

    pub async fn start_last_slot_delivered_listener(&self) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe(LAST_SLOT_DELIVERED_CHANNEL).await?;

        let mut message_stream = pubsub.on_message();

        while let Some(message) = message_stream.next().await {
            let Ok(slot) = message.get_payload::<u64>() else {
                warn!("Malformed Redis payload for last slot delivered");
                continue;
            };

            self.last_delivered_slot.store(slot, Ordering::Relaxed);
        }

        Ok(())
    }

    pub async fn start_builder_last_bid_received_at_listener(&self) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe(BUILDER_LAST_BID_RECEIVED_AT_CHANNEL).await?;

        let mut message_stream = pubsub.on_message();

        while let Some(message) = message_stream.next().await {
            let payload = message.get_payload::<String>().ok();

            if let Some(payload_str) = payload {
                if let Ok((key, received_at)) = from_str::<(String, u64)>(&payload_str) {
                    self.builder_latest_payload_received_at.insert(key, received_at);
                }
            }
        }

        Ok(())
    }

    pub async fn start_builder_last_bid_received_at_deleted_listener(
        &self,
    ) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe(BUILDER_LAST_BID_RECEIVED_AT_DELETED_CHANNEL).await?;

        let mut message_stream = pubsub.on_message();

        while let Some(message) = message_stream.next().await {
            let payload = message.get_payload::<String>().ok();
            if let Some(key) = payload {
                self.builder_latest_payload_received_at.remove(&key);
            }
        }

        Ok(())
    }

    pub async fn start_header_tx_root_listener(&self) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe(HEADER_TX_ROOT_CHANNEL).await?;

        let mut message_stream = pubsub.on_message();

        let mut conn = self.pool.get().await?;

        while let Some(message) = message_stream.next().await {
            let Ok((key, tx_root)) = Self::process_pubsub_update::<B256>(message, &mut conn).await
            else {
                continue;
            };

            self.header_tx_root_cache.insert(key, tx_root);
        }

        Ok(())
    }

    pub async fn start_builder_info_listener(&self) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe(BUILDER_INFO_CHANNEL).await?;

        let mut message_stream = pubsub.on_message();

        let mut conn = self.pool.get().await?;

        while let Some(message) = message_stream.next().await {
            let Ok((field, builder_info)) = Self::process_pubsub_update_hget::<BuilderInfo>(
                BUILDER_INFO_KEY,
                message,
                &mut conn,
            )
            .await
            else {
                continue;
            };

            self.builder_info_cache.insert(field, builder_info);
        }

        Ok(())
    }

    pub async fn start_floor_bid_listener(&self) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe(FLOOR_BIDS_CHANNEL).await?;

        let mut message_stream = pubsub.on_message();

        let mut conn = self.pool.get().await?;

        while let Some(message) = message_stream.next().await {
            let Ok((key, floor_value)) =
                Self::process_pubsub_update::<U256>(message, &mut conn).await
            else {
                continue;
            };

            self.floor_bid_value_cache.insert(key, floor_value);
        }

        Ok(())
    }

    async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let value: redis::Value = conn.get(key).await?;
        if let redis::Value::Nil = value {
            Ok(None)
        } else if let redis::Value::Data(data) = value {
            let deserialized: T = serde_json::from_slice(&data)?;
            Ok(Some(deserialized))
        } else {
            Err(RedisCacheError::UnexpectedValueType)
        }
    }

    async fn hget<T: DeserializeOwned>(
        &self,
        key: &str,
        field: &str,
    ) -> Result<Option<T>, RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let value: redis::Value = conn.hget(key, field).await?;
        if let redis::Value::Nil = value {
            Ok(None)
        } else if let redis::Value::Data(data) = value {
            let deserialized: T = serde_json::from_slice(&data)?;
            Ok(Some(deserialized))
        } else {
            Err(RedisCacheError::UnexpectedValueType)
        }
    }

    async fn hgetall<V: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<HashMap<String, V>>, RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let entries: HashMap<String, Vec<u8>> = conn.hgetall(key).await?;
        if entries.is_empty() {
            return Ok(None);
        }

        let mut deserialized_entries = HashMap::with_capacity(entries.len());
        for (key, value) in entries.into_iter() {
            let deserialized_value: V = serde_json::from_slice(&value)?;
            deserialized_entries.insert(key, deserialized_value);
        }

        Ok(Some(deserialized_entries))
    }

    async fn hgetall_raw(&self, key: &str) -> Result<HashMap<String, Vec<u8>>, RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let entries: HashMap<String, Vec<u8>> = conn.hgetall(key).await?;
        Ok(entries)
    }

    pub async fn scan_keys(&self, pattern: &str) -> Result<Vec<String>, RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let mut cursor: u64 = 0;
        let mut keys: Vec<String> = Vec::new();

        loop {
            let (new_cursor, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(1000)
                .query_async(&mut conn)
                .await?;

            keys.extend(batch);

            if new_cursor == 0 {
                break;
            }
            cursor = new_cursor;
        }

        Ok(keys)
    }

    #[allow(dead_code)]
    async fn lrange<T: DeserializeOwned>(
        &self,
        key: &str,
        start: isize,
        stop: isize,
    ) -> Result<Vec<T>, RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let values: Vec<Vec<u8>> = conn.lrange(key, start, stop).await?;

        let mut deserialized_values = Vec::with_capacity(values.len());
        for value in values.iter() {
            let deserialized: T = serde_json::from_slice(value)?;
            deserialized_values.push(deserialized);
        }

        Ok(deserialized_values)
    }

    async fn set(
        &self,
        key: &str,
        value: &impl Serialize,
        expiry: Option<usize>,
    ) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let str_val = serde_json::to_string(value)?;

        match expiry {
            Some(expiry) => Ok(conn.set_ex(key, str_val, expiry).await?),
            None => Ok(conn.set(key, str_val).await?),
        }
    }

    async fn publish(&self, channel: &str, key: &str) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        Ok(conn.publish(channel, key).await?)
    }

    async fn publish_json<T: Serialize>(
        &self,
        channel: &str,
        data: &T,
    ) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let json = serde_json::to_string(data)?;
        conn.publish(channel, json).await?;
        Ok(())
    }

    /// Attempts to set a lock in Redis with a specified key and expiry.
    ///
    /// This method uses an asynchronous Redis connection from a pool to execute the SET command.
    /// It employs a locking pattern where the lock is set only if the key doesn't already exist
    /// (`NX` option) and sets the lock to expire after a given duration (`expiry` in milliseconds).
    ///
    /// # Arguments
    /// * `key` - A reference to a string slice that holds the key for the lock.
    /// * `expiry` - The duration in milliseconds for which the lock should be valid.
    ///
    /// # Returns
    /// Returns `true` if the lock was successfully acquired, or `false` if the lock was not set
    async fn set_lock(&self, key: &str, id: &str, expiry: usize) -> bool {
        let mut conn = match self.pool.get().await {
            Ok(conn) => conn,
            Err(_) => return false,
        };

        let result: RedisResult<Value> = redis::cmd("SET")
            .arg(key)
            .arg(id)
            .arg("NX")
            .arg("PX")
            .arg(expiry)
            .query_async(&mut conn)
            .await;

        match result {
            Ok(Value::Okay) => true,
            Ok(_) | Err(_) => false,
        }
    }

    /// Attempts to renew an existing lock in Redis with a specified key and new expiry time.
    ///
    /// This method uses an asynchronous Redis connection from a pool to execute the SET command
    /// with the 'XX' option, ensuring that the lock is only renewed if it already exists.
    ///
    /// # Arguments
    /// * `key` - A reference to a string slice that holds the key of the lock to renew.
    /// * `expiry` - The new duration in milliseconds for which the lock should be valid.
    async fn renew_lock(&self, key: &str, id: &str, expiry: usize) -> bool {
        let mut conn = match self.pool.get().await {
            Ok(conn) => conn,
            Err(_) => return false,
        };

        let script = Script::new(RENEW_SCRIPT);
        let result =
            script.key(&[key]).arg(&[id, &expiry.to_string()]).invoke_async(&mut conn).await;

        match result {
            Ok(Value::Okay) => true,
            Ok(_) | Err(_) => false,
        }
    }

    async fn hset(
        &self,
        key: &str,
        field: &str,
        value: &impl Serialize,
    ) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let str_val = serde_json::to_string(value)?;
        Ok(conn.hset(key, field, str_val).await?)
    }

    async fn hset_multiple_not_exists<T: Serialize>(
        &self,
        key: &str,
        entries: &[(&str, T)],
        expiry: usize,
    ) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let mut pipeline = redis::pipe();

        // Iterate over the entries to serialize each value
        for (field, value) in entries {
            let str_val = serde_json::to_string(value)?;
            pipeline.cmd("HSETNX").arg(key).arg(field).arg(str_val);
        }

        pipeline.expire(key, expiry);

        Ok(pipeline.query_async(&mut conn).await?)
    }

    #[allow(dead_code)]
    async fn rpush(&self, key: &str, value: &impl Serialize) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let str_val = serde_json::to_string(value)?;
        Ok(conn.rpush(key, str_val).await?)
    }

    async fn hdel(&self, key: &str, field: &str) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        Ok(conn.hdel(key, field).await?)
    }

    #[allow(dead_code)]
    async fn clear_key(&self, key: &str) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        Ok(conn.del(key).await?)
    }

    async fn copy(
        &self,
        from: &str,
        to: &str,
        expiry: Option<usize>,
    ) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let c: i16 =
            redis::cmd("COPY").arg(from).arg(to).arg("REPLACE").query_async(&mut conn).await?;

        if c == 0 {
            return Err(RedisCacheError::RedisCopyError {
                from: from.to_string(),
                to: to.to_string(),
            });
        }

        // If an expiry is provided, set the expiry for the 'to' key
        if let Some(expiry_secs) = expiry {
            redis::cmd("EXPIRE").arg(to).arg(expiry_secs).query_async(&mut conn).await?;
        }

        Ok(())
    }

    async fn seen_or_add(
        &self,
        key: &str,
        entry: &impl Serialize,
    ) -> Result<bool, RedisCacheError> {
        let mut conn = self.pool.get().await?;

        let entry = serde_json::to_string(entry)?;
        let seen: bool = conn.sismember(key, &entry).await?;

        if !seen {
            conn.sadd(key, entry).await?;
            conn.expire(key, BID_CACHE_EXPIRY_S).await?;
        }

        Ok(seen)
    }

    async fn add(&self, key: &str, entry: String) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        conn.sadd(key, entry).await?;

        Ok(())
    }

    async fn remove(&self, key: &str, entries: Vec<String>) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        conn.srem(key, entries).await?;
        Ok(())
    }

    async fn get_set_members(&self, key: &str) -> Result<Vec<String>, RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let members: Vec<String> = conn.smembers(key).await?;
        Ok(members)
    }

    async fn get_new_builder_bids(
        &self,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<HashMap<String, U256>, RedisCacheError> {
        let key_latest_bids_value =
            get_builder_latest_bid_value_key(slot, parent_hash, proposer_pub_key);
        Ok(self.hgetall(&key_latest_bids_value).await?.unwrap_or_default())
    }

    /// Update the top bid based on the current state of builder bids.
    async fn update_top_bid(
        &self,
        state: &mut SaveBidAndUpdateTopBidResponse,
        builder_bids: &HashMap<String, U256>,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
        floor_value: U256,
    ) -> Result<(), RedisCacheError> {
        if builder_bids.is_empty() {
            return Ok(());
        }

        // Determine the current top bid.
        let (top_bid_builder_pub_key, top_builder_bid_value) =
            get_top_bid(builder_bids).ok_or(RedisCacheError::InternalError)?;

        // Use the floor value if it's greater than the top bid value.
        let (top_bid_value, top_bid_source_key) = if floor_value > top_builder_bid_value {
            (floor_value, get_floor_bid_key(slot, parent_hash, proposer_pub_key))
        } else {
            (
                top_builder_bid_value,
                get_latest_bid_by_builder_key_str_builder_pub_key(
                    slot,
                    parent_hash,
                    proposer_pub_key,
                    &top_bid_builder_pub_key,
                ),
            )
        };
        state.top_bid_value = top_bid_value;

        // Set the get header response to the new best bid
        let top_bid_key = get_cache_get_header_response_key(slot, parent_hash, proposer_pub_key);
        self.copy(&top_bid_source_key, &top_bid_key, Some(BID_CACHE_EXPIRY_S)).await?;

        // Update the global top bid value
        let top_bid_value_key = get_top_bid_value_key(slot, parent_hash, proposer_pub_key);
        self.top_bid_value_cache.insert(top_bid_value_key.clone(), state.top_bid_value);
        self.set(&top_bid_value_key, &state.top_bid_value, Some(BID_CACHE_EXPIRY_S)).await?;

        self.publish(BEST_BIDS_CHANNEL, &top_bid_key).await?;

        state.was_top_bid_updated = state.prev_top_bid_value != state.top_bid_value;

        Ok(())
    }

    async fn get_last_hash_delivered(&self) -> Result<Option<B256>, RedisCacheError> {
        self.get(LAST_HASH_DELIVERED_KEY).await
    }

    /// 1) Update floor bid payload by copying from the best builder bid to floor bid.
    /// 2) Update floor bid value.
    async fn set_new_floor(
        &self,
        new_floor_value: U256,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<(), RedisCacheError> {
        let key_bid_source =
            get_latest_bid_by_builder_key(slot, parent_hash, proposer_pub_key, builder_pub_key);
        let key_floor_bid = get_floor_bid_key(slot, parent_hash, proposer_pub_key);
        self.copy(&key_bid_source, &key_floor_bid, Some(BID_CACHE_EXPIRY_S)).await?;

        let key_floor_bid_value = get_floor_bid_value_key(slot, parent_hash, proposer_pub_key);
        self.floor_bid_value_cache.insert(key_floor_bid_value.clone(), new_floor_value);
        self.set(&key_floor_bid_value, &new_floor_value, Some(BID_CACHE_EXPIRY_S)).await?;

        self.publish(FLOOR_BIDS_CHANNEL, &key_floor_bid_value).await?;

        Ok(())
    }

    /// Process a redis pubsub update.
    /// Used when a channel just passes the key of an updated value rather than the whole value.
    async fn process_pubsub_update<T: DeserializeOwned>(
        update: Msg,
        conn: &mut Connection,
    ) -> Result<(String, T), RedisCacheError> {
        let key: String = update.get_payload().inspect_err(|err| {
            error!(%err, "Failed to get payload from message");
        })?;

        let data: String = conn.get(&key).await.inspect_err(|err| {
            error!(%err, "Failed to get data from redis");
        })?;

        let value = serde_json::from_str(&data).inspect_err(|err| {
            error!(%err, "Failed to deserialize data");
        })?;

        Ok((key, value))
    }

    async fn process_pubsub_update_hget<T: DeserializeOwned>(
        key: &str,
        update: Msg,
        conn: &mut Connection,
    ) -> Result<(String, T), RedisCacheError> {
        let field: String = update.get_payload().inspect_err(|err| {
            error!(%err, "Failed to get payload from message");
        })?;

        let data: String = conn.hget(key, &field).await.inspect_err(|err| {
            error!(%err, "Failed to get data from redis");
        })?;

        let value = serde_json::from_str(&data).inspect_err(|err| {
            error!(%err, "Failed to deserialize data");
        })?;

        Ok((field, value))
    }
}

#[async_trait]
impl Auctioneer for RedisCache {
    #[instrument(skip_all)]
    async fn get_last_slot_delivered(&self) -> Result<Option<u64>, AuctioneerError> {
        let mut record = RedisMetricRecord::new("get_last_slot_delivered");

        let last_slot_delivered = self.last_delivered_slot.load(Ordering::Relaxed);

        if last_slot_delivered > 0 {
            record.record_success();
            return Ok(Some(last_slot_delivered));
        }

        let mut conn = self.pool.get().await.map_err(RedisCacheError::from)?;
        let value = conn.get(LAST_SLOT_DELIVERED_KEY).await.map_err(RedisCacheError::from)?;

        record.record_success();
        Ok(value)
    }

    #[instrument(skip_all)]
    async fn check_and_set_last_slot_and_hash_delivered(
        &self,
        slot: u64,
        hash: &B256,
    ) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("check_and_set_last_slot_and_hash_delivered");

        let last_slot_delivered_res = self.get_last_slot_delivered().await?;

        if let Some(last_slot_delivered) = last_slot_delivered_res {
            if slot < last_slot_delivered {
                return Err(AuctioneerError::PastSlotAlreadyDelivered);
            }

            if slot == last_slot_delivered {
                let last_hash_delivered_res = self.get_last_hash_delivered().await?;

                match last_hash_delivered_res {
                    Some(last_hash_delivered) => {
                        if *hash != last_hash_delivered {
                            return Err(AuctioneerError::AnotherPayloadAlreadyDeliveredForSlot);
                        }
                    }
                    None => return Err(AuctioneerError::UnexpectedValueType),
                }

                record.record_success();
                return Ok(());
            }
        }

        self.last_delivered_slot.store(slot, Ordering::Relaxed);

        let mut conn = self.pool.get().await.map_err(RedisCacheError::from)?;
        let mut pipe = redis::pipe();

        // Add the SET commands to the pipeline
        let slot_value = serde_json::to_string(&slot).map_err(RedisCacheError::from)?;
        let hash_value =
            serde_json::to_string(&format!("{hash:?}")).map_err(RedisCacheError::from)?;
        pipe.atomic()
            .cmd("SET")
            .arg(LAST_SLOT_DELIVERED_KEY)
            .arg(&slot_value)
            .ignore()
            .cmd("SET")
            .arg(LAST_HASH_DELIVERED_KEY)
            .arg(hash_value)
            .ignore();

        pipe.query_async(&mut conn).await.map_err(RedisCacheError::from)?;

        self.publish(LAST_SLOT_DELIVERED_CHANNEL, slot_value.as_str()).await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_best_bid(
        &self,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<SignedBuilderBid>, AuctioneerError> {
        let mut record = RedisMetricRecord::new("get_best_bid");

        let key = get_cache_get_header_response_key(slot, parent_hash, proposer_pub_key);
        let wrapped_bid: Option<SignedBuilderBidWrapper> = self.get(&key).await?;

        record.record_success();
        Ok(wrapped_bid.map(|wrapped_bid| wrapped_bid.bid))
    }

    #[instrument(skip_all)]
    fn get_best_bids(&self) -> broadcast::Receiver<Bytes> {
        self.tx.subscribe()
    }

    #[instrument(skip_all)]
    async fn save_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &B256,
        execution_payload: &PayloadAndBlobs,
    ) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("save_execution_payload");

        let key = get_execution_payload_key(slot, proposer_pub_key, block_hash);
        self.set(&key, &execution_payload, Some(BID_CACHE_EXPIRY_S)).await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &B256,
        fork_name: ForkName,
    ) -> Result<Option<PayloadAndBlobs>, AuctioneerError> {
        let mut record = RedisMetricRecord::new("get_execution_payload");

        let key = get_execution_payload_key(slot, proposer_pub_key, block_hash);
        let execution_payload = self.get::<PayloadAndBlobs>(&key).await?.map(|p| PayloadAndBlobs {
            execution_payload: maybe_upgrade_execution_payload(p.execution_payload, fork_name),
            blobs_bundle: p.blobs_bundle,
        });

        record.record_success();
        Ok(execution_payload)
    }

    #[instrument(skip_all)]
    async fn get_bid_trace(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &B256,
    ) -> Result<Option<BidTrace>, AuctioneerError> {
        let mut record = RedisMetricRecord::new("get_bid_trace");

        let key = get_cache_bid_trace_key(slot, proposer_pub_key, block_hash);
        let bid_trace = self.get(&key).await?;

        record.record_success();
        Ok(bid_trace)
    }

    #[instrument(skip_all)]
    async fn save_bid_trace(&self, bid_trace: &BidTrace) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("save_bid_trace");

        let key = get_cache_bid_trace_key(
            bid_trace.slot,
            &bid_trace.proposer_pubkey,
            &bid_trace.block_hash,
        );
        self.set(&key, &bid_trace, Some(BID_CACHE_EXPIRY_S)).await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_builder_latest_payload_received_at(
        &self,
        slot: u64,
        builder_pub_key: &BlsPublicKey,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<u64>, AuctioneerError> {
        let mut record = RedisMetricRecord::new("get_builder_latest_payload_received_at");

        let key =
            get_latest_bid_by_builder_key(slot, parent_hash, proposer_pub_key, builder_pub_key);
        if let Some(cached) = self.builder_latest_payload_received_at.get(&key) {
            record.record_success();
            return Ok(Some(cached));
        }

        let key = get_builder_latest_bid_time_key(slot, parent_hash, proposer_pub_key);
        let received_at = self.hget(&key, &format!("{builder_pub_key:?}")).await?;

        record.record_success();
        Ok(received_at)
    }

    /// This function performs three operations:
    /// 1. Stores the full `SignedBuilderBid` object.
    /// 2. Stores the time at which this bid was received.
    /// 3. Stores the value of the bid.
    #[instrument(skip_all)]
    async fn save_builder_bid(
        &self,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
        builder_pub_key: &BlsPublicKey,
        received_at: u128,
        builder_bid: &SignedBuilderBid,
    ) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("save_builder_bid");

        let mut conn = self.pool.get().await.map_err(RedisCacheError::from)?;
        let mut pipe = redis::pipe();

        let wrapped_builder_bid = SignedBuilderBidWrapper::new(
            builder_bid.clone(),
            slot,
            builder_pub_key.clone(),
            received_at,
        );

        let serialised_bid =
            serde_json::to_string(&wrapped_builder_bid).map_err(RedisCacheError::from)?;
        let serialised_value = serde_json::to_string(&builder_bid.data.message.value())
            .map_err(RedisCacheError::from)?;

        let key_latest_bid =
            get_latest_bid_by_builder_key(slot, parent_hash, proposer_pub_key, builder_pub_key);
        let key_latest_bids_time =
            get_builder_latest_bid_time_key(slot, parent_hash, proposer_pub_key);
        let key_latest_bids_value =
            get_builder_latest_bid_value_key(slot, parent_hash, proposer_pub_key);
        let builder_pub_key_str = format!("{:?}", builder_pub_key);

        pipe.atomic()
            // Store the full SignedBuilderBid object with expiry
            .cmd("SET")
            .arg(&key_latest_bid)
            .arg(serialised_bid)
            .arg("EX")
            .arg(BID_CACHE_EXPIRY_S)
            .ignore()
            // Store the time at which this bid was received with expiry
            .hset(&key_latest_bids_time, &builder_pub_key_str, received_at as u64)
            .ignore()
            .expire(&key_latest_bids_time, BID_CACHE_EXPIRY_S)
            .ignore()
            // Store the value of the bid with expiry
            .hset(&key_latest_bids_value, &builder_pub_key_str, serialised_value)
            .ignore()
            .expire(&key_latest_bids_value, BID_CACHE_EXPIRY_S)
            .ignore();

        pipe.query_async(&mut conn).await.map_err(RedisCacheError::from)?;

        self.builder_latest_payload_received_at.insert(key_latest_bid.clone(), received_at as u64);
        let payload = (key_latest_bid.clone(), received_at as u64);
        self.publish_json(BUILDER_LAST_BID_RECEIVED_AT_CHANNEL, &payload).await?;

        record.record_success();
        Ok(())
    }

    /// The `save_bid_and_update_top_bid` function performs several key operations:
    ///
    /// 1. It first fetches the latest bids for a particular slot, parent hash, and proposer.
    /// 2. It then updates the top bid based on these fetched bids and a given floor value.
    /// 3. It saves the current submission as a new bid.
    /// 4. Optionally, it updates the floor value if the submission value is above the floor.
    #[instrument(skip_all)]
    async fn save_bid_and_update_top_bid(
        &self,
        submission: &SignedBidSubmission,
        received_at: u128,
        cancellations_enabled: bool,
        floor_value: U256,
        state: &mut SaveBidAndUpdateTopBidResponse,
        signing_context: &RelaySigningContext,
    ) -> Result<Option<(SignedBuilderBid, PayloadAndBlobs)>, AuctioneerError> {
        let mut record = RedisMetricRecord::new("save_bid_and_update_top_bid");

        // Exit early if cancellations aren't enabled and the bid is below the floor.
        let is_bid_above_floor = submission.bid_trace().value > floor_value;
        if !cancellations_enabled && !is_bid_above_floor {
            record.record_success();
            return Ok(None);
        }

        // Save the execution payload
        self.save_execution_payload(
            submission.slot().as_u64(),
            submission.proposer_public_key(),
            submission.block_hash(),
            &submission.payload_and_blobs(),
        )
        .await?;
        state.set_latency_save_payload();

        // Sign builder bid with relay pubkey.
        let builder_bid = bid_submission_to_builder_bid(submission, signing_context);

        // Save builder bid and update top bid/ floor keys if possible.
        self.save_signed_builder_bid_and_update_top_bid(
            &builder_bid,
            submission.message(),
            received_at,
            cancellations_enabled,
            floor_value,
            state,
        )
        .await?;

        record.record_success();
        Ok(Some((builder_bid, submission.payload_and_blobs())))
    }

    #[instrument(skip_all)]
    async fn get_top_bid_value(
        &self,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<U256>, AuctioneerError> {
        let mut record = RedisMetricRecord::new("get_top_bid_value");

        let key = get_top_bid_value_key(slot, parent_hash, proposer_pub_key);

        if let Some(cached) = self.top_bid_value_cache.get(&key) {
            record.record_success();
            return Ok(Some(cached));
        }

        let top_bid_value = self.get(&key).await?;
        if let Some(val) = top_bid_value {
            self.top_bid_value_cache.insert(key.clone(), val);
        }

        record.record_success();
        Ok(top_bid_value)
    }

    #[instrument(skip_all)]
    async fn get_floor_bid_value(
        &self,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<U256>, AuctioneerError> {
        let mut record = RedisMetricRecord::new("get_floor_bid_value");

        let key = get_floor_bid_value_key(slot, parent_hash, proposer_pub_key);
        if let Some(cached) = self.floor_bid_value_cache.get(&key) {
            record.record_success();
            return Ok(Some(cached));
        }
        let floor_bid_value = self.get(&key).await?;
        if let Some(value) = floor_bid_value {
            self.floor_bid_value_cache.insert(key.clone(), value);
        }

        record.record_success();
        Ok(floor_bid_value)
    }

    #[instrument(skip_all)]
    async fn delete_builder_bid(
        &self,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("delete_builder_bid");

        // Delete the value
        let key_latest_value =
            get_builder_latest_bid_value_key(slot, parent_hash, proposer_pub_key);
        self.hdel(&key_latest_value, &format!("{builder_pub_key:?}")).await?;

        // Delete the time
        let key_latest_time = get_builder_latest_bid_time_key(slot, parent_hash, proposer_pub_key);
        self.hdel(&key_latest_time, &format!("{builder_pub_key:?}")).await?;
        self.publish(
            BUILDER_LAST_BID_RECEIVED_AT_DELETED_CHANNEL,
            &get_latest_bid_by_builder_key(slot, parent_hash, proposer_pub_key, builder_pub_key),
        )
        .await?;

        // Update bids now to determine current top bid
        let mut state = SaveBidAndUpdateTopBidResponse::default();

        let builder_bids = self.get_new_builder_bids(slot, parent_hash, proposer_pub_key).await?;
        let floor_value = self
            .get_floor_bid_value(slot, parent_hash, proposer_pub_key)
            .await?
            .unwrap_or(U256::ZERO);

        self.update_top_bid(
            &mut state,
            &builder_bids,
            slot,
            parent_hash,
            proposer_pub_key,
            floor_value,
        )
        .await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_builder_info(
        &self,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<BuilderInfo, AuctioneerError> {
        let mut record = RedisMetricRecord::new("get_builder_info");
        let key = format!("{builder_pub_key:?}");
        if let Some(cached) = self.builder_info_cache.get(&key) {
            record.record_success();
            return Ok(cached);
        }

        let builder_info = self
            .hget(BUILDER_INFO_KEY, &key)
            .await?
            .ok_or(AuctioneerError::BuilderNotFound { pub_key: builder_pub_key.clone() })?;

        record.record_success();
        Ok(builder_info)
    }

    #[instrument(skip_all)]
    async fn demote_builder(&self, builder_pub_key: &BlsPublicKey) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("demote_builder");
        let mut builder_info = self.get_builder_info(builder_pub_key).await?;
        if !builder_info.is_optimistic {
            return Ok(());
        }
        builder_info.is_optimistic = false;
        builder_info.is_optimistic_for_regional_filtering = false;
        self.builder_info_cache.insert(format!("{builder_pub_key:?}"), builder_info.clone());
        self.hset(BUILDER_INFO_KEY, &format!("{builder_pub_key:?}"), &builder_info).await?;
        self.publish(BUILDER_INFO_CHANNEL, &format!("{builder_pub_key:?}")).await?;

        // TODO remove pending blocks

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn update_builder_infos(
        &self,
        builder_infos: &[BuilderInfoDocument],
    ) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("update_builder_infos");

        if builder_infos.is_empty() {
            record.record_success();
            return Ok(());
        }

        // Fetch current builder info
        let redis_builder_infos: HashMap<String, BuilderInfo> =
            self.hgetall(BUILDER_INFO_KEY).await?.unwrap_or_default();

        // Update Redis value if the builder info has changed or it's a new builder
        for builder_info in builder_infos {
            let builder_pub_key_str = format!("{:?}", builder_info.pub_key);
            if let Some(redis_builder_info) = redis_builder_infos.get(&builder_pub_key_str) {
                if builder_info.builder_info != *redis_builder_info {
                    self.builder_info_cache
                        .insert(builder_pub_key_str.clone(), builder_info.builder_info.clone());
                    self.hset(BUILDER_INFO_KEY, &builder_pub_key_str, &builder_info.builder_info)
                        .await?;
                    self.publish(BUILDER_INFO_CHANNEL, &builder_pub_key_str).await?;
                    info!(
                        pubkey = %builder_info.pub_key,
                        is_optimistic = builder_info.builder_info.is_optimistic,
                        "updated builder info",
                    );
                }
            } else {
                self.builder_info_cache
                    .insert(builder_pub_key_str.clone(), builder_info.builder_info.clone());
                self.hset(BUILDER_INFO_KEY, &builder_pub_key_str, &builder_info.builder_info)
                    .await?;
                self.publish(BUILDER_INFO_CHANNEL, &builder_pub_key_str).await?;
                info!(
                    pubkey = %builder_info.pub_key,
                    is_optimistic = builder_info.builder_info.is_optimistic,
                    "updated builder info",
                );
            }
        }

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn seen_or_insert_block_hash(
        &self,
        block_hash: &B256,
        slot: u64,
        parent_hash: &B256,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<bool, AuctioneerError> {
        let mut record = RedisMetricRecord::new("seen_or_insert_block_hash");

        let key = get_seen_block_hashes_key(slot, parent_hash, proposer_pub_key, block_hash);

        if self.seen_block_hashes.contains_key(&key) {
            record.record_success();
            return Ok(true);
        }

        self.seen_block_hashes.insert(key.clone(), ());
        self.publish(SEEN_BLOCK_HASHES_CHANNEL, &key).await?;
        record.record_success();
        Ok(false)
    }

    #[instrument(skip_all)]
    async fn save_signed_builder_bid_and_update_top_bid(
        &self,
        builder_bid: &SignedBuilderBid,
        bid_trace: &BidTrace,
        received_at: u128,
        cancellations_enabled: bool,
        floor_value: U256,
        state: &mut SaveBidAndUpdateTopBidResponse,
    ) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("save_signed_builder_bid_and_update_top_bid");

        // Exit early if cancellations aren't enabled and the bid is below the floor.
        let is_bid_above_floor = builder_bid.data.message.value() > &floor_value;
        if !cancellations_enabled && !is_bid_above_floor {
            record.record_success();
            return Ok(());
        }

        // Load the latest bids from all builders for the current slot, parent hash, and proposer
        let mut builder_bids = self
            .get_new_builder_bids(
                bid_trace.slot,
                &bid_trace.parent_hash,
                &bid_trace.proposer_pubkey,
            )
            .await?;

        // Get the current top bid. It will be the max of all builder bids and the current floor.
        state.top_bid_value =
            builder_bids.values().max().cloned().unwrap_or(U256::ZERO).max(floor_value);

        state.prev_top_bid_value = state.top_bid_value;

        state.set_latency_get_prev_top_bid();

        // Save the latest bid for this builder
        self.save_builder_bid(
            bid_trace.slot,
            &bid_trace.parent_hash,
            &bid_trace.proposer_pubkey,
            &bid_trace.builder_pubkey,
            received_at,
            builder_bid,
        )
        .await?;
        state.was_bid_saved = true;
        builder_bids
            .insert(format!("{:?}", bid_trace.builder_pubkey), *builder_bid.data.message.value());
        state.set_latency_save_bid();

        // Save the bid trace
        self.save_bid_trace(bid_trace).await?;
        state.set_latency_save_trace();

        // Abort if the top bid hasn't changed
        // TODO: the floor may have raised but we will exit early here.
        state.top_bid_value = builder_bids.values().max().cloned().unwrap_or(U256::ZERO);
        if state.top_bid_value == state.prev_top_bid_value {
            record.record_success();
            return Ok(());
        }

        // Update the top bid
        self.update_top_bid(
            state,
            &builder_bids,
            bid_trace.slot,
            &bid_trace.parent_hash,
            &bid_trace.proposer_pubkey,
            floor_value,
        )
        .await?;
        state.is_new_top_bid = builder_bid.data.message.value() == &state.top_bid_value;
        state.set_latency_update_top_bid();

        // Handle floor value updates only if needed.
        // Only non-cancellable bids above the floor should set a new floor.
        if cancellations_enabled || !is_bid_above_floor {
            record.record_success();
            return Ok(());
        }
        self.set_new_floor(
            *builder_bid.data.message.value(),
            bid_trace.slot,
            &bid_trace.parent_hash,
            &bid_trace.proposer_pubkey,
            &bid_trace.builder_pubkey,
        )
        .await?;
        state.set_latency_update_floor();

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_header_tx_root(&self, block_hash: &B256) -> Result<Option<B256>, AuctioneerError> {
        let mut record = RedisMetricRecord::new("get_header_tx_root");
        let key = get_header_tx_root_key(block_hash);
        if let Some(cached) = self.header_tx_root_cache.get(&key) {
            record.record_success();
            return Ok(Some(cached));
        }

        let tx_root = self.get(&key).await?;

        record.record_success();
        Ok(tx_root)
    }

    #[instrument(skip_all)]
    async fn save_header_submission_and_update_top_bid(
        &self,
        submission: &SignedHeaderSubmission,
        received_at: u128,
        cancellations_enabled: bool,
        floor_value: U256,
        state: &mut SaveBidAndUpdateTopBidResponse,
        signing_context: &RelaySigningContext,
    ) -> Result<Option<SignedBuilderBid>, AuctioneerError> {
        let mut record = RedisMetricRecord::new("save_header_submission_and_update_top_bid");

        // Exit early if cancellations aren't enabled and the bid is below the floor.
        let is_bid_above_floor = submission.value() > floor_value;
        if !cancellations_enabled && !is_bid_above_floor {
            record.record_success();
            return Ok(None);
        }

        // Cache the transaction root for the header
        let key = get_header_tx_root_key(submission.block_hash());
        self.header_tx_root_cache.insert(key.clone(), submission.transactions_root());
        self.set(&key, &submission.transactions_root(), Some(24)).await?;
        self.publish(HEADER_TX_ROOT_CHANNEL, &key).await?;

        // Sign builder bid with relay pubkey.
        let builder_bid = header_submission_to_builder_bid(submission, signing_context);

        // Save builder bid and update top bid/ floor keys if possible.
        self.save_signed_builder_bid_and_update_top_bid(
            &builder_bid,
            submission.bid_trace(),
            received_at,
            cancellations_enabled,
            floor_value,
            state,
        )
        .await?;

        record.record_success();
        Ok(Some(builder_bid))
    }

    #[instrument(skip_all)]
    async fn update_trusted_proposers(
        &self,
        proposer_whitelist: Vec<ProposerInfo>,
    ) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("update_trusted_proposers");

        // get keys
        let proposer_keys: Vec<String> =
            proposer_whitelist.iter().map(|proposer| format!("{:?}", proposer.pubkey)).collect();

        // add or update proposers
        for proposer in proposer_whitelist {
            let key_str = format!("{:?}", proposer.pubkey);
            self.hset(PROPOSER_WHITELIST_KEY, &key_str, &proposer).await?;
        }

        // remove any proposers that are no longer in the list
        let proposer_info: Option<HashMap<String, ProposerInfo>> =
            self.hgetall(PROPOSER_WHITELIST_KEY).await?;

        if let Some(proposer_info) = proposer_info {
            for key in proposer_info.keys() {
                if !proposer_keys.contains(key) {
                    self.hdel(PROPOSER_WHITELIST_KEY, key).await?;
                }
            }
        }

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn is_trusted_proposer(
        &self,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<bool, AuctioneerError> {
        let mut record = RedisMetricRecord::new("is_trusted_proposer");

        let key_str = format!("{proposer_pub_key:?}");
        let proposer_info: Option<ProposerInfo> =
            self.hget(PROPOSER_WHITELIST_KEY, &key_str).await?;

        record.record_success();
        Ok(proposer_info.is_some())
    }

    #[instrument(skip_all)]
    async fn update_primev_proposers(
        &self,
        primev_proposers: &[BlsPublicKey],
    ) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("update_primev_proposers");

        // get keys
        let proposer_keys: Vec<String> =
            primev_proposers.iter().map(|proposer| format!("{:?}", proposer)).collect();

        // add or update proposers
        for proposer in primev_proposers {
            let key_str = format!("{:?}", proposer);
            self.hset(PRIMEV_PROPOSERS_KEY, &key_str, &proposer).await?;
        }

        // remove any proposers that are no longer in the list
        let proposer_info: Option<HashMap<String, BlsPublicKey>> =
            self.hgetall(PRIMEV_PROPOSERS_KEY).await?;

        if let Some(proposer_info) = proposer_info {
            for key in proposer_info.keys() {
                if !proposer_keys.contains(key) {
                    self.hdel(PRIMEV_PROPOSERS_KEY, key).await?;
                }
            }
        }

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn is_primev_proposer(
        &self,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<bool, AuctioneerError> {
        let mut record = RedisMetricRecord::new("is_primev_proposer");

        let key_str = format!("{proposer_pub_key:?}");
        let proposer_info: Option<BlsPublicKey> = self.hget(PRIMEV_PROPOSERS_KEY, &key_str).await?;
        let is_primev = proposer_info.is_some();

        record.record_success();
        Ok(is_primev)
    }

    #[instrument(skip_all)]
    async fn get_payload_url(
        &self,
        block_hash: &B256,
    ) -> Result<Option<(BlsPublicKey, Vec<u8>)>, AuctioneerError> {
        let mut record = RedisMetricRecord::new("get_payload_url");
        let key = format!("{PAYLOAD_ADDRESS_KEY}_{block_hash:?}");
        let payload_address: Option<(BlsPublicKey, Vec<u8>)> = self.get(&key).await?;
        record.record_success();
        Ok(payload_address)
    }

    #[instrument(skip_all)]
    async fn save_payload_address(
        &self,
        block_hash: &B256,
        builder_pub_key: &BlsPublicKey,
        payload_socket_address: Vec<u8>,
    ) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("save_payload_address");
        let key = format!("{PAYLOAD_ADDRESS_KEY}_{block_hash:?}");
        self.set(
            &key,
            &(builder_pub_key.clone(), payload_socket_address),
            Some(PAYLOAD_ADDRESS_EXPIRY_S),
        )
        .await?;
        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn save_pending_block_header(
        &self,
        slot: u64,
        builder_pub_key: &BlsPublicKey,
        block_hash: &B256,
        timestamp_ms: u64,
    ) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("save_pending_block_header");

        let key = get_pending_block_builder_block_hash_key(builder_pub_key, block_hash);
        let entries = vec![("slot", slot), ("header_received", timestamp_ms)];
        self.hset_multiple_not_exists(key.as_str(), &entries, PENDING_BLOCK_EXPIRY_S).await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn save_pending_block_payload(
        &self,
        slot: u64,
        builder_pub_key: &BlsPublicKey,
        block_hash: &B256,
        timestamp_ms: u64,
    ) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("save_pending_block_payload");

        let key = get_pending_block_builder_block_hash_key(builder_pub_key, block_hash);
        let entries = vec![("slot", slot), ("payload_received", timestamp_ms)];
        self.hset_multiple_not_exists(key.as_str(), &entries, PENDING_BLOCK_EXPIRY_S).await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_pending_blocks(&self) -> Result<Vec<PendingBlock>, AuctioneerError> {
        let mut record = RedisMetricRecord::new("get_pending_blocks");

        let mut pending_blocks: Vec<PendingBlock> = Vec::new();

        let redis_builder_infos: Option<HashMap<String, BuilderInfo>> =
            self.hgetall(BUILDER_INFO_KEY).await?;

        let Some(redis_builder_infos) = redis_builder_infos else {
            record.record_success();
            return Ok(pending_blocks);
        };

        for (bulder_pub_key_str, builder_info) in redis_builder_infos {
            if builder_info.is_optimistic {
                let builder_pubkey = get_pubkey_from_hex(&bulder_pub_key_str)?;
                let builder_key_prefix =
                    format!("{}_*", get_pending_block_builder_key(&builder_pubkey));

                let keys: Vec<String> = self.scan_keys(builder_key_prefix.as_str()).await?;

                for key in keys {
                    let pending_block = self.hgetall_raw(key.as_str()).await?;

                    if pending_block.is_empty() {
                        continue;
                    }

                    let block_hash_str = key.split('_').last().unwrap_or("");
                    let block_hash = get_hash_from_hex(block_hash_str)?;

                    let slot = match pending_block.get("slot") {
                        Some(s) => serde_json::from_slice(s).map_err(RedisCacheError::from)?,
                        None => 0,
                    };

                    let header_received = match pending_block.get("header_received") {
                        Some(s) => Some(serde_json::from_slice(s).map_err(RedisCacheError::from)?),
                        None => None,
                    };

                    let payload_received = match pending_block.get("payload_received") {
                        Some(s) => Some(serde_json::from_slice(s).map_err(RedisCacheError::from)?),
                        None => None,
                    };

                    let pending_block = PendingBlock {
                        slot,
                        block_hash,
                        builder_pubkey: builder_pubkey.clone(),
                        header_receive_ms: header_received,
                        payload_receive_ms: payload_received,
                    };

                    pending_blocks.push(pending_block);
                }
            }
        }

        record.record_success();
        Ok(pending_blocks)
    }

    /// Attempts to acquire or renew leadership for a distributed task based on the current
    /// leadership status.
    ///
    /// If the instance is already a leader (indicated by the `leader` argument), it attempts to
    /// renew the lock. If the lock renewal is successful, it returns `true`.
    ///
    /// If the instance is not currently a leader or fails to renew the lock, it attempts to acquire
    /// the lock. The function returns `true` if the lock acquisition is successful, indicating
    /// leadership has been obtained.
    ///
    /// Expiry is set to `HOUSEKEEPER_LOCK_EXPIRY_MS` milliseconds to ensure that the lock is
    /// released if the instance crashes. `HOUSEKEEPER_LOCK_EXPIRY_MS`` should be long enought
    /// to allow the leader to renew the lock before it expires.
    ///
    /// Arguments:
    /// - `leader`: A unique id for this relay.
    ///
    /// Returns:
    /// - `true` if the instance is the leader and successfully renews the lock, or if it
    ///   successfully acquires the lock.
    /// - `false` if it fails to renew or acquire the lock.
    ///
    /// Note: This function assumes that the caller manages and passes the current leadership
    /// status.
    async fn try_acquire_or_renew_leadership(&self, leader_id: &str) -> bool {
        if self.renew_lock(HOUSEKEEPER_LOCK_KEY, leader_id, HOUSEKEEPER_LOCK_EXPIRY_MS).await {
            return true;
        }

        return self.set_lock(HOUSEKEEPER_LOCK_KEY, leader_id, HOUSEKEEPER_LOCK_EXPIRY_MS).await;
    }

    async fn kill_switch_enabled(&self) -> Result<bool, AuctioneerError> {
        let kill_switch: Option<bool> = self.get(KILL_SWITCH).await?;
        Ok(kill_switch.unwrap_or_default())
    }

    async fn enable_kill_switch(&self) -> Result<(), AuctioneerError> {
        self.set(KILL_SWITCH, &true, None).await?;
        Ok(())
    }

    async fn disable_kill_switch(&self) -> Result<(), AuctioneerError> {
        self.set(KILL_SWITCH, &false, None).await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn update_current_inclusion_list(
        &self,
        inclusion_list: InclusionListWithMetadata,
        slot_coordinate: String,
    ) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("update_current_inclusion_list");

        let new_inclusion_list_key = format!("{CURRENT_INCLUSION_LIST_KEY}_{slot_coordinate}");

        self.set(&new_inclusion_list_key, &inclusion_list, Some(INCLUSION_LIST_EXPIRY_S)).await?;

        self.publish(CURRENT_INCLUSION_LIST_CHANNEL, &new_inclusion_list_key).await?;

        record.record_success();

        Ok(())
    }

    #[instrument(skip_all)]
    fn get_inclusion_list(&self) -> broadcast::Receiver<InclusionListWithKey> {
        self.inclusion_list.subscribe()
    }

    #[instrument(skip_all)]
    async fn publish_head_event(&self, head_event: &HeadEventData) -> Result<(), AuctioneerError> {
        self.publish_json("head_event_channel", &head_event).await?;
        Ok(())
    }

    fn get_head_event(&self) -> broadcast::Receiver<HeadEventData> {
        self.head_event.subscribe()
    }

    #[instrument(skip_all)]
    async fn publish_payload_attributes(
        &self,
        payload_attributes: &PayloadAttributesEvent,
    ) -> Result<(), AuctioneerError> {
        self.publish_json("payload_attributes_channel", &payload_attributes).await?;
        Ok(())
    }

    fn get_payload_attributes(&self) -> broadcast::Receiver<PayloadAttributesEvent> {
        self.payload_attributes.subscribe()
    }

    #[instrument(skip_all)]
    async fn update_proposer_duties(
        &self,
        duties: Vec<BuilderGetValidatorsResponseEntry>,
    ) -> Result<(), AuctioneerError> {
        self.set("proposer_duties", &duties, Some(60 * 60)).await?; // 1 hour expiry
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_proposer_duties(
        &self,
    ) -> Result<Vec<BuilderGetValidatorsResponseEntry>, AuctioneerError> {
        let duties: Option<Vec<BuilderGetValidatorsResponseEntry>> =
            self.get("proposer_duties").await?;
        Ok(duties.unwrap_or_default())
    }
}

#[derive(Clone, Debug)]
pub struct InclusionListWithKey {
    pub key: String,
    pub inclusion_list: InclusionListWithMetadata,
}

impl<'a> From<&'a InclusionListWithKey> for (&'a InclusionListWithMetadata, &'a str) {
    fn from(value: &'a InclusionListWithKey) -> Self {
        (&value.inclusion_list, &value.key)
    }
}

fn get_top_bid(bid_values: &HashMap<String, U256>) -> Option<(String, U256)> {
    bid_values.iter().max_by_key(|&(_, value)| value).map(|(key, value)| (key.clone(), *value))
}

#[cfg(test)]
mod tests {
    use std::clone;

    use alloy_primitives::U256;
    use helix_common::utils::utcnow_ns;
    use helix_types::{
        get_fixed_pubkey, BlsSignature, BuilderBid, BuilderBidElectra, ExecutionPayloadElectra,
        ExecutionPayloadHeaderElectra, ForkName, KzgCommitments, SignedBidSubmissionElectra,
        SignedBuilderBidInner, TestRandomSeed,
    };
    use serde::{Deserialize, Serialize};
    use serial_test::serial;

    use super::*;

    impl RedisCache {
        async fn clear_cache(&self) -> Result<(), RedisCacheError> {
            let mut conn = self.pool.get().await?;
            redis::cmd("FLUSHALL")
                .query_async(&mut conn)
                .await
                .map_err(RedisCacheError::RedisError)?;
            Ok(())
        }
    }

    /// #######################################################################
    /// ########################### RedisCache tests ##########################
    /// #######################################################################
    ///
    /// Note: These tests require a running Redis server. You can run a Redis server using Docker:
    /// ```
    /// docker pull redis/redis-stack-server:latest
    /// docker run -d --name redis-stack-server -p 6379:6379 redis/redis-stack-server:latest
    /// ```
    ///
    /// Reference: https://redis.io/kb/doc/1hcec8xg9w/how-can-i-install-redis-on-docker

    #[tokio::test]
    #[serial]
    async fn test_new() {
        let result = RedisCache::new("redis://127.0.0.1/", Vec::new()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn test_get_and_set_object() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct Sample {
            field: String,
        }

        let sample = Sample { field: "test".to_string() };

        // Test: Set object
        let set_result = cache.set("test_key", &sample, None).await;
        assert!(set_result.is_ok(), "Failed to set object.");

        // Test: Get object
        let get_result: Result<Option<Sample>, _> = cache.get("test_key").await;
        assert!(get_result.is_ok(), "Failed to get the object");
        assert!(get_result.as_ref().unwrap().is_some(), "Object was None");
        assert_eq!(get_result.unwrap().unwrap(), sample, "Object mismatch");
    }

    #[tokio::test]
    #[serial]
    async fn test_hget_and_hset_object() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let value = "test_value";
        let field = "test_field";
        let key = "test_key";

        // Test: Hset object
        let set_result = cache.hset(key, field, &value).await;
        assert!(set_result.is_ok(), "Failed to hset value");

        let get_result: Result<Option<String>, _> = cache.hget(key, field).await;
        assert!(get_result.is_ok(), "Failed to hget the value");
        assert!(get_result.as_ref().unwrap().is_some(), "Value was None");
        assert_eq!(get_result.unwrap().unwrap(), value, "Value mismatch");
    }

    #[tokio::test]
    #[serial]
    async fn test_hgetall() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let field_val_pairs: HashMap<String, String> = [
            ("field1".to_string(), "value1".to_string()),
            ("field2".to_string(), "value2".to_string()),
        ]
        .iter()
        .cloned()
        .collect();

        // Hset all objects
        for (field, value) in &field_val_pairs {
            cache.hset("test_hgetall_key", field, value).await.unwrap();
        }

        // Test: Hgetall
        let get_result: Result<Option<HashMap<String, String>>, _> =
            cache.hgetall("test_hgetall_key").await;
        assert!(get_result.is_ok(), "Failed to hgetall");
        assert!(get_result.as_ref().unwrap().is_some(), "Hgetall returned None");
        assert_eq!(get_result.unwrap().unwrap().len(), field_val_pairs.len(), "Hgetall mismatch");
    }

    #[tokio::test]
    #[serial]
    async fn test_lrange() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let values = vec!["value1", "value2", "value3"];

        // Test: lpush
        for value in &values {
            let lpush_res = cache.rpush("test_lrange_key", value).await;
            assert!(lpush_res.is_ok(), "Failed to lpush");
        }

        let get_result: Result<Vec<String>, _> = cache.lrange("test_lrange_key", 0, -1).await;
        assert!(get_result.is_ok(), "Failed to lrange");
        assert_eq!(get_result.unwrap().len(), values.len(), "Values mismatch");
    }

    #[tokio::test]
    #[serial]
    async fn test_rpush() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let value = "test_value";
        let set_result = cache.rpush("test_rpush_key", &value).await;
        assert!(set_result.is_ok(), "Failed to rpush");
    }

    #[tokio::test]
    #[serial]
    async fn test_clear_key() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let key = "test_clear_key";
        let value = "some_value";

        // Set a key-value pair
        cache.set(key, &value, None).await.unwrap();

        // Test: Clear the key
        let clear_result = cache.clear_key(key).await;
        assert!(clear_result.is_ok(), "Failed to clear key");

        // Validate: the key has been cleared
        let get_result: Result<Option<String>, _> = cache.get(key).await;
        assert!(get_result.unwrap().is_none(), "Key was not cleared");
    }

    #[tokio::test]
    #[serial]
    async fn test_get_new_builder_bids() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        // Current slot key
        let slot = 5_u64;
        let parent_hash = B256::default();
        let proposer_pub_key = BlsPublicKey::test_random();
        let key = get_builder_latest_bid_value_key(slot, &parent_hash, &proposer_pub_key);

        // Bid info
        let pub_key_1 = BlsPublicKey::test_random();
        let pub_key_2 = BlsPublicKey::test_random();
        let pub_key_3 = BlsPublicKey::test_random();

        let bids = vec![
            (pub_key_1, U256::from(50)),
            (pub_key_2, U256::from(51)),
            (pub_key_3, U256::from(90)),
        ];
        // Save all bids
        for (pub_key, value) in bids {
            let set_res = cache.hset(&key, &format!("{pub_key:?}"), &value).await;
            assert!(set_res.is_ok(), "Failed to hset");
        }

        let fetched_bids = cache.get_new_builder_bids(slot, &parent_hash, &proposer_pub_key).await;
        assert!(fetched_bids.is_ok(), "Failed to fetch new builder bids");
        assert_eq!(fetched_bids.unwrap().len(), 3, "Number of bids mismatch");
    }

    #[tokio::test]
    #[serial]
    async fn test_update_top_bid() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let mut state = SaveBidAndUpdateTopBidResponse {
            top_bid_value: U256::ZERO,
            prev_top_bid_value: U256::ZERO,
            was_top_bid_updated: false,
            ..Default::default()
        };

        // Block info
        let slot = 23894;
        let parent_hash = B256::default();
        let proposer_pub_key = BlsPublicKey::test_random();

        // Save prev best bid
        let prev_builder_pubkey = BlsPublicKey::test_random();
        let mut builder_bid = BuilderBidElectra {
            pubkey: prev_builder_pubkey.clone().into(),
            header: ExecutionPayloadHeaderElectra::test_random(),
            value: U256::from(60),
            blob_kzg_commitments: Default::default(),
            execution_requests: Default::default(),
        };

        let prev_best_bid =
            SignedBuilderBid::new_no_metadata(Some(ForkName::Electra), SignedBuilderBidInner {
                message: builder_bid.clone().into(),
                signature: BlsSignature::test_random(),
            });

        let res = cache
            .save_builder_bid(
                slot,
                &parent_hash,
                &proposer_pub_key,
                &prev_builder_pubkey,
                23,
                &prev_best_bid,
            )
            .await;
        assert!(res.is_ok(), "Failed to save prev best bid");

        // Test with empty builder_bids
        let empty_bids = HashMap::new();
        let floor_value = U256::from(40);
        let res = cache
            .update_top_bid(
                &mut state,
                &empty_bids,
                slot,
                &parent_hash,
                &proposer_pub_key,
                floor_value,
            )
            .await;
        assert!(res.is_ok(), "Function should handle empty bids");

        // Populate builder_bids
        let mut builder_bids = HashMap::new();
        builder_bids.insert(format!("{prev_builder_pubkey:?}"), U256::from(60));
        builder_bids.insert("builder1".to_string(), U256::from(50));
        builder_bids.insert("builder2".to_string(), U256::from(30));
        builder_bids.insert("builder3".to_string(), U256::from(40));

        // Test updating the top bid
        let res = cache
            .update_top_bid(
                &mut state,
                &builder_bids,
                slot,
                &parent_hash,
                &proposer_pub_key,
                floor_value,
            )
            .await;
        assert!(res.is_ok(), "Failed to update top bid");
        assert_eq!(state.top_bid_value, U256::from(60), "Top bid value mismatch");
        assert!(state.was_top_bid_updated, "Top bid should be updated");

        // Test the Redis cache
        let _key_top_bid = get_top_bid_value_key(slot, &parent_hash, &proposer_pub_key);
        let mut _conn = cache.pool.get().await.unwrap();

        let cache_value =
            cache.get_top_bid_value(slot, &parent_hash, &proposer_pub_key).await.unwrap();
        assert_eq!(cache_value, Some(U256::from(60)), "Cache value mismatch");

        // Test with floor_value greater than top_bid_value
        let higher_floor_value = U256::from(70);

        builder_bid.value = higher_floor_value;
        let floor_bid =
            SignedBuilderBid::new_no_metadata(Some(ForkName::Electra), SignedBuilderBidInner {
                message: builder_bid.into(),
                signature: BlsSignature::test_random(),
            });

        let key_floor_bid = get_floor_bid_key(slot, &parent_hash, &proposer_pub_key);
        let res = cache.set(&key_floor_bid, &floor_bid, None).await;
        assert!(res.is_ok(), "Failed to set floor bid");

        let res = cache
            .update_top_bid(
                &mut state,
                &builder_bids,
                slot,
                &parent_hash,
                &proposer_pub_key,
                higher_floor_value,
            )
            .await;
        assert!(res.is_ok(), "Failed to set top bid with higher floor_value");
        assert_eq!(state.top_bid_value, U256::from(70), "Top bid value should be floor_value");
        assert!(state.was_top_bid_updated, "Top bid should be updated with floor_value");

        // Test the Redis cache
        let cache_value: Option<U256> =
            cache.get_top_bid_value(slot, &parent_hash, &proposer_pub_key).await.unwrap();
        assert_eq!(cache_value, Some(U256::from(70)), "Cache value mismatch with floor_value");
    }

    /// #######################################################################
    /// ########################### Auctioneer tests ##########################
    /// #######################################################################

    #[tokio::test]
    #[serial]
    async fn test_get_and_check_last_slot_and_hash_delivered() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let block_hash = B256::try_from([4u8; 32].as_ref()).unwrap();

        // Test: Save the last slot and hash delivered
        let set_result = cache.check_and_set_last_slot_and_hash_delivered(slot, &block_hash).await;
        assert!(set_result.is_ok(), "Saving last slot and hash delivered failed");

        // Test: Get the last slot delivered
        let get_result: Result<Option<u64>, _> = cache.get_last_slot_delivered().await;
        assert!(get_result.is_ok(), "Fetching last slot delivered failed");
        assert_eq!(get_result.unwrap().unwrap(), slot, "Slot value mismatch");
    }

    #[tokio::test]
    #[serial]
    async fn test_set_past_slot() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let block_hash = B256::try_from([4u8; 32].as_ref()).unwrap();

        // Set a future slot
        assert!(cache
            .check_and_set_last_slot_and_hash_delivered(slot + 1, &block_hash)
            .await
            .is_ok());

        // Test: Try to set a past slot
        let set_result = cache.check_and_set_last_slot_and_hash_delivered(slot, &block_hash).await;
        assert!(matches!(set_result, Err(AuctioneerError::PastSlotAlreadyDelivered)));
    }

    #[tokio::test]
    #[serial]
    async fn test_set_same_slot_different_hash() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let block_hash1 = B256::try_from([4u8; 32].as_ref()).unwrap();
        let block_hash2 = B256::try_from([5u8; 32].as_ref()).unwrap();

        // Set initial slot and hash
        assert!(cache.check_and_set_last_slot_and_hash_delivered(slot, &block_hash1).await.is_ok());

        // Test: Set the same slot with a different hash
        let set_result = cache.check_and_set_last_slot_and_hash_delivered(slot, &block_hash2).await;
        assert!(matches!(set_result, Err(AuctioneerError::AnotherPayloadAlreadyDeliveredForSlot)));
    }

    #[tokio::test]
    #[serial]
    async fn test_set_same_slot_no_hash() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let block_hash = B256::try_from([4u8; 32].as_ref()).unwrap();

        // Set just the slot but not the hash
        assert!(cache.set(LAST_SLOT_DELIVERED_KEY, &slot, None).await.is_ok());

        // Test: Set the same slot
        let set_result = cache.check_and_set_last_slot_and_hash_delivered(slot, &block_hash).await;
        assert!(matches!(set_result, Err(AuctioneerError::UnexpectedValueType)));
    }

    #[tokio::test]
    #[serial]
    async fn test_get_and_set_best_bid() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let parent_hash = B256::default();
        let proposer_pub_key = BlsPublicKey::test_random();

        let bid =
            SignedBuilderBid::new_no_metadata(Some(ForkName::Electra), SignedBuilderBidInner {
                message: BuilderBid::Electra(BuilderBidElectra {
                    value: U256::from(1999),
                    header: ExecutionPayloadHeaderElectra::test_random(),
                    blob_kzg_commitments: KzgCommitments::test_random(),
                    pubkey: BlsPublicKey::test_random().into(),
                    execution_requests: Default::default(),
                }),
                signature: BlsSignature::test_random(),
            });

        let best_bid = bid.into();

        let wrapper = SignedBuilderBidWrapper::new(best_bid, slot, proposer_pub_key.clone(), 0);

        // Save the best bid
        let key = get_cache_get_header_response_key(slot, &parent_hash, &proposer_pub_key);
        let set_result = cache.set(&key, &wrapper, None).await;
        assert!(set_result.is_ok(), "Failed to set best bid in cache");

        // Test: Get the best bid
        let get_result: Result<Option<SignedBuilderBid>, _> =
            cache.get_best_bid(slot, &parent_hash, &proposer_pub_key).await;
        assert!(get_result.is_ok(), "Failed to get the best bid");
        assert!(get_result.as_ref().unwrap().is_some(), "Best bid was None");

        let fetched_builder_bid = get_result.unwrap().unwrap();
        assert_eq!(
            fetched_builder_bid.data.message.value(),
            &U256::from(1999),
            "Best bid value mismatch"
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_get_and_save_execution_payload() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let proposer_pub_key = BlsPublicKey::test_random();
        let block_hash = B256::test_random();

        let payload = ExecutionPayloadElectra { gas_limit: 999, ..Default::default() };
        let versioned_execution_payload =
            PayloadAndBlobs { execution_payload: payload.into(), blobs_bundle: Default::default() };

        // Save the execution payload
        let save_result = cache
            .save_execution_payload(
                slot,
                &proposer_pub_key,
                &block_hash,
                &versioned_execution_payload,
            )
            .await;
        assert!(save_result.is_ok(), "Failed to save the execution payload");

        // Test: Get the execution payload
        let get_result: Result<Option<PayloadAndBlobs>, _> = cache
            .get_execution_payload(slot, &proposer_pub_key, &block_hash, ForkName::Electra)
            .await;
        assert!(get_result.is_ok(), "Failed to get the execution payload");
        assert!(get_result.as_ref().unwrap().is_some(), "Execution payload is None");

        let fetched_execution_payload = get_result.unwrap().unwrap();
        assert_eq!(
            fetched_execution_payload.execution_payload.gas_limit(),
            999,
            "Execution payload mismatch"
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_save_builder_bid_and_get_latest_payload_received_at() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        // Test data
        let slot = 1;
        let parent_hash = B256::default();
        let proposer_pub_key = BlsPublicKey::test_random();
        let builder_pub_key = BlsPublicKey::test_random();
        let received_at = 1616237123000u128;
        let value = U256::from(100);
        let block_hash = B256::try_from([4u8; 32].as_ref()).unwrap();

        let mut header = ExecutionPayloadHeaderElectra::test_random();
        header.block_hash = block_hash.into();
        let bid =
            SignedBuilderBid::new_no_metadata(Some(ForkName::Electra), SignedBuilderBidInner {
                message: BuilderBid::Electra(BuilderBidElectra {
                    value,
                    header,
                    blob_kzg_commitments: KzgCommitments::test_random(),
                    pubkey: BlsPublicKey::test_random().into(),
                    execution_requests: Default::default(),
                }),
                signature: BlsSignature::test_random(),
            });

        let builder_bid = bid.into();

        // Test: save_builder_bid
        let res = cache
            .save_builder_bid(
                slot,
                &parent_hash,
                &proposer_pub_key,
                &builder_pub_key,
                received_at,
                &builder_bid,
            )
            .await;
        assert!(res.is_ok(), "Failed to execute save_builder_bid");

        // Validate: the SignedBuilderBid object is correctly set
        let key_latest_bid =
            get_latest_bid_by_builder_key(slot, &parent_hash, &proposer_pub_key, &builder_pub_key);
        let fetched_bid: Result<Option<SignedBuilderBidWrapper>, _> =
            cache.get(&key_latest_bid).await;
        assert!(fetched_bid.is_ok(), "Failed to fetch the latest bid");

        let fetched_bid = fetched_bid.unwrap().unwrap().bid;

        assert_eq!(
            fetched_bid.data.message.header().block_hash(),
            builder_bid.data.message.header().block_hash(),
            "Mismatch in saved builder bid"
        );

        // Test: get_builder_latest_payload_received_at
        let fetched_time = cache
            .get_builder_latest_payload_received_at(
                slot,
                &builder_pub_key,
                &parent_hash,
                &proposer_pub_key,
            )
            .await;
        // Validate: Correct time was fetched
        assert!(fetched_time.is_ok(), "Failed to get_builder_latest_payload_received_at");
        assert_eq!(fetched_time.unwrap().unwrap(), received_at as u64, "Mismatch in saved time");

        // Validate the value is correctly set
        let key_latest_bids_value =
            get_builder_latest_bid_value_key(slot, &parent_hash, &proposer_pub_key);
        let fetched_value: Result<Option<U256>, _> =
            cache.hget(&key_latest_bids_value, &format!("{builder_pub_key:?}")).await;
        assert!(fetched_value.is_ok(), "Failed to fetch the latest bid value");
        assert_eq!(fetched_value.unwrap().unwrap(), value, "Mismatch in saved value");
    }

    #[tokio::test]
    async fn test_get_floor_bid_value() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let parent_hash = B256::default();
        let proposer_pub_key = BlsPublicKey::test_random();
        let floor_bid_value = U256::from(1000);

        // Set the floor value
        let key = get_floor_bid_value_key(slot, &parent_hash, &proposer_pub_key);
        let set_result = cache.set(&key, &floor_bid_value, None).await;
        assert!(set_result.is_ok(), "Failed to set the floor value");

        // Test: Get the floor value
        let get_result: Result<Option<U256>, _> =
            cache.get_floor_bid_value(slot, &parent_hash, &proposer_pub_key).await;
        assert!(get_result.is_ok(), "Failed to get the floor value");
        assert!(get_result.as_ref().unwrap().is_some(), "Floor value is None");
        assert_eq!(get_result.unwrap().unwrap(), floor_bid_value, "Floor value mismatch");
    }

    #[tokio::test]
    #[serial]
    async fn test_get_builder_info() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key = BlsPublicKey::test_random();
        let unknown_builder_pub_key = BlsPublicKey::test_random();

        let builder_info = BuilderInfo {
            collateral: U256::from(12),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: false,
            builder_id: None,
            builder_ids: None,
        };

        // Test case 1: Builder exists
        let set_result =
            cache.hset(BUILDER_INFO_KEY, &format!("{builder_pub_key:?}"), &builder_info).await;
        assert!(set_result.is_ok(), "Failed to set builder info");

        let get_result = cache.get_builder_info(&builder_pub_key).await;
        assert!(get_result.is_ok(), "Failed to get builder info");
        assert_eq!(
            get_result.unwrap().collateral,
            builder_info.collateral,
            "Builder info mismatch"
        );

        // Test case 2: Builder doesn't exist
        let result = cache.get_builder_info(&unknown_builder_pub_key).await;
        assert!(result.is_err(), "Fetched builder info for unknown builder");
        assert!(
            matches!(result.unwrap_err(), AuctioneerError::BuilderNotFound { .. }),
            "Incorrect get builder info error"
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_get_trusted_proposers_and_update_trusted_proposers() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let is_trusted = cache.is_trusted_proposer(&BlsPublicKey::test_random()).await.unwrap();
        assert!(!is_trusted, "Failed to check trusted proposer");

        cache
            .update_trusted_proposers(vec![
                ProposerInfo { name: "test".to_string(), pubkey: get_fixed_pubkey(0) },
                ProposerInfo { name: "test2".to_string(), pubkey: get_fixed_pubkey(1) },
            ])
            .await
            .unwrap();

        let is_trusted = cache.is_trusted_proposer(&get_fixed_pubkey(0)).await.unwrap();
        assert!(is_trusted, "Failed to check trusted proposer");

        let is_trusted = cache.is_trusted_proposer(&get_fixed_pubkey(1)).await.unwrap();
        assert!(is_trusted, "Failed to check trusted proposer");

        let is_trusted = cache.is_trusted_proposer(&get_fixed_pubkey(2)).await.unwrap();
        assert!(!is_trusted, "Failed to check trusted proposer");

        cache
            .update_trusted_proposers(vec![ProposerInfo {
                name: "test2".to_string(),
                pubkey: get_fixed_pubkey(3),
            }])
            .await
            .unwrap();

        let is_trusted = cache.is_trusted_proposer(&BlsPublicKey::test_random()).await.unwrap();
        assert!(!is_trusted, "Failed to check trusted proposer");

        let is_trusted = cache.is_trusted_proposer(&get_fixed_pubkey(3)).await.unwrap();
        assert!(is_trusted, "Failed to check trusted proposer");
    }

    #[tokio::test]
    async fn test_demote_non_optimistic_builder() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key = BlsPublicKey::test_random();
        let builder_info = BuilderInfo {
            collateral: U256::from(12),
            is_optimistic: false,
            is_optimistic_for_regional_filtering: false,
            builder_id: None,
            builder_ids: None,
        };

        // Set builder info in the cache
        let set_result =
            cache.hset(BUILDER_INFO_KEY, &format!("{builder_pub_key:?}"), &builder_info).await;
        assert!(set_result.is_ok(), "Failed to set builder info");

        // Test: Demote builder
        let result = cache.demote_builder(&builder_pub_key).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn test_demote_optimistic_builder() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key_optimistic = BlsPublicKey::test_random();
        let builder_info = BuilderInfo {
            collateral: U256::from(12),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: false,
            builder_id: None,
            builder_ids: None,
        };

        // Set builder info in the cache
        let set_result = cache
            .hset(BUILDER_INFO_KEY, &format!("{builder_pub_key_optimistic:?}"), &builder_info)
            .await;
        assert!(set_result.is_ok(), "Failed to set builder info");
        assert!(
            cache.get_builder_info(&builder_pub_key_optimistic).await.unwrap().is_optimistic,
            "Builder is not optimistic after setting"
        );

        // Test: Demote builder
        let result = cache.demote_builder(&builder_pub_key_optimistic).await;
        assert!(result.is_ok());

        // Validate: builder is no longer optimistic
        assert!(!cache.get_builder_info(&builder_pub_key_optimistic).await.unwrap().is_optimistic);
    }

    #[tokio::test]
    #[serial]
    async fn test_delete_builder_bid() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        // Default vals
        let slot = 1;
        let parent_hash = B256::default();
        let proposer_pub_key = BlsPublicKey::test_random();
        let received_at = 12;

        // Save 2 builder bids. builder bid 1 > builder bid 2
        let builder_pub_key_1 = BlsPublicKey::test_random();
        let builder_bid_1 =
            SignedBuilderBid::new_no_metadata(Some(ForkName::Electra), SignedBuilderBidInner {
                message: BuilderBidElectra {
                    value: U256::from(100),
                    header: ExecutionPayloadHeaderElectra::test_random(),
                    blob_kzg_commitments: KzgCommitments::test_random(),
                    pubkey: BlsPublicKey::test_random().into(),
                    execution_requests: Default::default(),
                }
                .into(),
                signature: BlsSignature::test_random(),
            });

        let builder_pub_key_2 = BlsPublicKey::test_random();
        let mut builder_bid_2 = builder_bid_1.clone();
        *builder_bid_2.data.message.value_mut() = U256::from(50);

        // Save both builder bids
        let set_result = cache
            .save_builder_bid(
                slot,
                &parent_hash,
                &proposer_pub_key,
                &builder_pub_key_1,
                received_at,
                &builder_bid_1,
            )
            .await;
        assert!(set_result.is_ok(), "Failed to save builder bid 1");

        let set_result = cache
            .save_builder_bid(
                slot,
                &parent_hash,
                &proposer_pub_key,
                &builder_pub_key_2,
                received_at,
                &builder_bid_2,
            )
            .await;
        assert!(set_result.is_ok(), "Failed to save builder bid 2");

        // Builder bid 1 should be the top bid
        let mut state = SaveBidAndUpdateTopBidResponse::default();
        let builder_bids =
            cache.get_new_builder_bids(slot, &parent_hash, &proposer_pub_key).await.unwrap();
        let floor_value = cache
            .get_floor_bid_value(slot, &parent_hash, &proposer_pub_key)
            .await
            .unwrap()
            .unwrap_or(U256::ZERO);

        let update_res = cache
            .update_top_bid(
                &mut state,
                &builder_bids,
                slot,
                &parent_hash,
                &proposer_pub_key,
                floor_value,
            )
            .await;
        assert!(update_res.is_ok(), "Failed to update top bid");

        let top_bid = cache.get_best_bid(slot, &parent_hash, &proposer_pub_key).await;
        assert!(top_bid.is_ok(), "Failed to get best bid");
        assert_eq!(
            top_bid.unwrap().unwrap().data.message.value(),
            &U256::from(100),
            "Top bid mismatch"
        );

        // Test: Delete best builder bid
        let delete_result = cache
            .delete_builder_bid(slot, &parent_hash, &proposer_pub_key, &builder_pub_key_1)
            .await;
        assert!(delete_result.is_ok(), "Failed to delete builder bid");

        // Validate: builder bid 2 is now the best bid
        let top_bid = cache.get_best_bid(slot, &parent_hash, &proposer_pub_key).await;
        assert!(top_bid.is_ok(), "Failed to get best bid");
        assert_eq!(
            top_bid.unwrap().unwrap().data.message.value(),
            &U256::from(50),
            "Top bid mismatch"
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_no_cancellation_bid_below_floor() {
        let (cache, submission, floor_value, received_at) = setup_save_and_update_test().await;
        let mut state = SaveBidAndUpdateTopBidResponse::default();

        let result = cache
            .save_bid_and_update_top_bid(
                &submission,
                received_at,
                false,
                floor_value,
                &mut state,
                &RelaySigningContext::default(),
            )
            .await;
        assert!(result.is_ok(), "Save failed");
        assert!(!state.was_bid_saved);
        assert!(!state.is_new_top_bid);
    }

    #[tokio::test]
    #[serial]
    async fn test_no_cancellation_bid_above_floor() {
        let (cache, mut submission, floor_value, received_at) = setup_save_and_update_test().await;
        let mut state = SaveBidAndUpdateTopBidResponse::default();

        submission.message_mut().value = floor_value + U256::from(1);
        let result = cache
            .save_bid_and_update_top_bid(
                &submission,
                received_at,
                false,
                floor_value,
                &mut state,
                &RelaySigningContext::default(),
            )
            .await;
        assert!(result.is_ok(), "Save failed");
        assert!(state.was_bid_saved, "Bid should be saved");
        assert!(state.is_new_top_bid, "Bid should be new top bid");

        // Validate bid is new floor
        let new_floor_value = cache
            .get_floor_bid_value(
                submission.message().slot,
                &submission.message().parent_hash,
                &submission.message().proposer_pubkey,
            )
            .await
            .unwrap()
            .unwrap_or(U256::ZERO);
        assert!(new_floor_value > floor_value, "Floor value should increase");
    }

    #[tokio::test]
    #[serial]
    async fn test_cancellation_bid_below_floor() {
        let (cache, mut submission, floor_value, received_at) = setup_save_and_update_test().await;
        let mut state = SaveBidAndUpdateTopBidResponse::default();

        submission.message_mut().value = floor_value.saturating_sub(U256::from(1));
        let result = cache
            .save_bid_and_update_top_bid(
                &submission,
                received_at,
                true,
                floor_value,
                &mut state,
                &RelaySigningContext::default(),
            )
            .await;
        assert!(result.is_ok(), "Save failed");
        assert!(state.was_bid_saved, "Bid should be saved");
        assert!(!state.is_new_top_bid, "Bid should not be new top bid");

        // Validate floor is the same
        let new_floor_value = cache
            .get_floor_bid_value(
                submission.message().slot,
                &submission.message().parent_hash,
                &submission.message().proposer_pubkey,
            )
            .await
            .unwrap()
            .unwrap_or(U256::ZERO);
        assert!(new_floor_value == floor_value, "Floor value should not change");
    }

    #[tokio::test]
    #[serial]
    async fn test_cancellation_bid_above_floor() {
        let (cache, mut submission, floor_value, received_at) = setup_save_and_update_test().await;
        let mut state = SaveBidAndUpdateTopBidResponse::default();

        submission.message_mut().value = floor_value + U256::from(1);
        let result = cache
            .save_bid_and_update_top_bid(
                &submission,
                received_at,
                true,
                floor_value,
                &mut state,
                &RelaySigningContext::default(),
            )
            .await;
        assert!(result.is_ok(), "Failed to save bid");
        assert!(state.was_bid_saved, "Bid should be saved");
        assert!(state.is_new_top_bid, "Bid should be new top bid");

        // Validate bid is not new floor as this is a cancellable bid
        let new_floor_value = cache
            .get_floor_bid_value(
                submission.message().slot,
                &submission.message().parent_hash,
                &submission.message().proposer_pubkey,
            )
            .await
            .unwrap()
            .unwrap_or(U256::ZERO);
        assert!(new_floor_value != submission.message().value, "Floor value should not change");
    }

    #[tokio::test]
    #[serial]
    async fn test_no_cancellation_bid_above_floor_but_not_top() {
        let (cache, mut submission, floor_value, received_at) = setup_save_and_update_test().await;

        // Save top bid from different builder. Cancellations enabled so won't set new floor.
        submission.message_mut().builder_pubkey = BlsPublicKey::test_random();
        submission.message_mut().value = floor_value + U256::from(2);
        let mut state = SaveBidAndUpdateTopBidResponse::default();
        let result = cache
            .save_bid_and_update_top_bid(
                &submission,
                received_at,
                true,
                floor_value,
                &mut state,
                &RelaySigningContext::default(),
            )
            .await;
        assert!(result.is_ok(), "Failed to save top bid");

        // Save bid below top bid but above floor.
        submission.message_mut().value = floor_value + U256::from(1);
        submission.message_mut().builder_pubkey = BlsPublicKey::test_random();
        let mut state = SaveBidAndUpdateTopBidResponse::default();

        let result = cache
            .save_bid_and_update_top_bid(
                &submission,
                received_at,
                false,
                floor_value,
                &mut state,
                &RelaySigningContext::default(),
            )
            .await;
        assert!(result.is_ok(), "Failed to save bid");
        assert!(state.was_bid_saved, "Bid should be saved");
        assert!(!state.is_new_top_bid, "Bid should not be the new top bid");
    }

    async fn setup_save_and_update_test() -> (RedisCache, SignedBidSubmission, U256, u128) {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let floor_value = U256::from(50);
        let received_at = 1000;

        let mut state = SaveBidAndUpdateTopBidResponse::default();
        let mut submission = SignedBidSubmissionElectra::test_random();

        // Save floor value

        submission.message.value = floor_value;
        cache
            .save_bid_and_update_top_bid(
                &submission.clone().into(),
                received_at,
                false,
                U256::ZERO,
                &mut state,
                &RelaySigningContext::default(),
            )
            .await
            .unwrap();

        // Reset submission values
        submission.message.builder_pubkey = BlsPublicKey::test_random();
        submission.message.value = U256::from(10);

        (cache, submission.into(), floor_value, received_at)
    }

    #[tokio::test]
    #[serial]
    async fn test_seen_or_insert_block_hash() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        let cloned_cache = cache.clone();
        tokio::spawn(async move {
            cloned_cache.start_seen_block_hashes_listener().await.unwrap();
        });
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let block_hash = B256::random();
        let pubkey = get_fixed_pubkey(0);

        // Test: Check if block hash has been seen before (should be false initially)
        let seen_result =
            cache.seen_or_insert_block_hash(&block_hash, slot, &B256::default(), &pubkey).await;
        assert!(seen_result.is_ok(), "Failed to check if block hash was seen");
        assert!(!seen_result.unwrap(), "Block hash was incorrectly seen before");

        // Test: Insert the block hash and check again (should be true after insert)
        let seen_result_again =
            cache.seen_or_insert_block_hash(&block_hash, slot, &B256::default(), &pubkey).await;
        assert!(seen_result_again.is_ok(), "Failed to check if block hash was seen after insert");
        assert!(seen_result_again.unwrap(), "Block hash was not seen after insert");

        // Test: Add a different new block hash (should be false initially)
        let block_hash_2 = B256::random();
        let seen_result =
            cache.seen_or_insert_block_hash(&block_hash_2, slot, &B256::default(), &pubkey).await;
        assert!(seen_result.is_ok(), "Failed to check if block hash was seen");
        assert!(!seen_result.unwrap(), "Block hash was incorrectly seen before");

        // Test: Insert the original block hash again, ensure it wasn't overwritten (should be true
        // after insert)
        let seen_result_again =
            cache.seen_or_insert_block_hash(&block_hash, slot, &B256::default(), &pubkey).await;
        assert!(seen_result_again.is_ok(), "Failed to check if block hash was seen after insert");
        assert!(seen_result_again.unwrap(), "Block hash was not seen after insert");
    }

    #[tokio::test]
    #[serial]
    async fn test_can_aquire_lock() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();
        assert!(cache.try_acquire_or_renew_leadership("leader").await)
    }

    #[tokio::test]
    #[serial]
    async fn test_others_cant_aquire_lock_if_held() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
        assert!(!cache.try_acquire_or_renew_leadership("others").await);
    }

    #[tokio::test]
    #[serial]
    async fn test_can_renew_lock() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
    }

    #[tokio::test]
    #[serial]
    async fn test_others_cannot_renew() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
        assert!(!cache.try_acquire_or_renew_leadership("others").await);
    }

    #[tokio::test]
    #[serial]
    async fn test_pending_blocks() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key = BlsPublicKey::test_random();
        let builder_infos = vec![BuilderInfoDocument {
            builder_info: BuilderInfo {
                collateral: U256::from(100),
                is_optimistic: true,
                is_optimistic_for_regional_filtering: false,
                builder_id: None,
                builder_ids: None,
            },
            pub_key: builder_pub_key.clone(),
        }];

        cache.update_builder_infos(&builder_infos).await.unwrap();

        let slot = 42;
        let block_hash = B256::try_from([5u8; 32].as_ref()).unwrap();

        let time = 1616237123000u64;

        cache.save_pending_block_header(slot, &builder_pub_key, &block_hash, time).await.unwrap();

        cache.save_pending_block_payload(slot, &builder_pub_key, &block_hash, time).await.unwrap();

        let pending_blocks = cache.get_pending_blocks().await.unwrap();
        assert_eq!(pending_blocks.len(), 1);

        for i in pending_blocks {
            assert_eq!(i.slot, slot);
            assert_eq!(i.block_hash, block_hash);
            assert_eq!(i.builder_pubkey, builder_pub_key);
            assert_eq!(i.header_receive_ms, Some(time));
            assert_eq!(i.payload_receive_ms, Some(time));
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_pending_blocks_multiple() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key = BlsPublicKey::test_random();
        let builder_infos = vec![BuilderInfoDocument {
            builder_info: BuilderInfo {
                collateral: U256::from(100),
                is_optimistic: true,
                is_optimistic_for_regional_filtering: false,
                builder_id: None,
                builder_ids: None,
            },
            pub_key: builder_pub_key.clone(),
        }];

        cache.update_builder_infos(&builder_infos).await.unwrap();

        let mut expected_pending_blocks = Vec::new();

        for i in 0..10 {
            let slot = i as u64;
            let block_hash = B256::try_from([i; 32].as_ref()).unwrap();

            let time = utcnow_ns();

            cache
                .save_pending_block_header(slot, &builder_pub_key, &block_hash, time)
                .await
                .unwrap();

            cache
                .save_pending_block_payload(slot, &builder_pub_key, &block_hash, time)
                .await
                .unwrap();

            expected_pending_blocks.push(PendingBlock {
                slot,
                block_hash,
                builder_pubkey: builder_pub_key.clone(),
                header_receive_ms: Some(time),
                payload_receive_ms: Some(time),
            });
        }

        let mut pending_blocks = cache.get_pending_blocks().await.unwrap();
        assert_eq!(pending_blocks.len(), 10);
        pending_blocks.sort_by_key(|block| block.slot);

        for (actual, expected) in pending_blocks.iter().zip(expected_pending_blocks.iter()) {
            assert_eq!(actual.slot, expected.slot);
            assert_eq!(actual.block_hash, expected.block_hash);
            assert_eq!(actual.builder_pubkey, expected.builder_pubkey);
            assert_eq!(actual.header_receive_ms, expected.header_receive_ms);
            assert_eq!(actual.payload_receive_ms, expected.payload_receive_ms);
        }

        let mut pending_block_hashes = HashMap::new();
        for pending_block in pending_blocks {
            pending_block_hashes
                .entry(pending_block.builder_pubkey.clone())
                .or_insert_with(Vec::new)
                .push(pending_block.block_hash.clone());
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_pending_blocks_no_header() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key = BlsPublicKey::test_random();
        let builder_infos = vec![BuilderInfoDocument {
            builder_info: BuilderInfo {
                collateral: U256::from(100),
                is_optimistic: true,
                is_optimistic_for_regional_filtering: false,
                builder_id: None,
                builder_ids: None,
            },
            pub_key: builder_pub_key.clone(),
        }];

        cache.update_builder_infos(&builder_infos).await.unwrap();

        let slot = 42;
        let block_hash = B256::try_from([5u8; 32].as_ref()).unwrap();
        let time = 1616237123000u64;

        cache.save_pending_block_payload(slot, &builder_pub_key, &block_hash, time).await.unwrap();

        let pending_blocks = cache.get_pending_blocks().await.unwrap();
        assert_eq!(pending_blocks.len(), 1);

        for i in pending_blocks {
            assert_eq!(i.slot, slot);
            assert_eq!(i.block_hash, block_hash);
            assert_eq!(i.builder_pubkey, builder_pub_key);
            assert_eq!(i.header_receive_ms, None);
            assert_eq!(i.payload_receive_ms, Some(time));
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_pending_blocks_no_payload() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key = BlsPublicKey::test_random();
        let builder_infos = vec![BuilderInfoDocument {
            builder_info: BuilderInfo {
                collateral: U256::from(100),
                is_optimistic: true,
                is_optimistic_for_regional_filtering: false,
                builder_id: None,
                builder_ids: None,
            },
            pub_key: builder_pub_key.clone(),
        }];

        cache.update_builder_infos(&builder_infos).await.unwrap();

        let slot = 42;
        let block_hash = B256::try_from([5u8; 32].as_ref()).unwrap();

        let time = 1616237123000u64;

        cache.save_pending_block_header(slot, &builder_pub_key, &block_hash, time).await.unwrap();

        let pending_blocks = cache.get_pending_blocks().await.unwrap();
        assert_eq!(pending_blocks.len(), 1);
        for i in pending_blocks {
            assert_eq!(i.slot, slot);
            assert_eq!(i.block_hash, block_hash);
            assert_eq!(i.builder_pubkey, builder_pub_key);
            assert_eq!(i.header_receive_ms, Some(time));
            assert_eq!(i.payload_receive_ms, None);
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_pending_blocks_dublicate_payload() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key = BlsPublicKey::test_random();

        let builder_infos = vec![BuilderInfoDocument {
            builder_info: BuilderInfo {
                collateral: U256::from(100),
                is_optimistic: true,
                is_optimistic_for_regional_filtering: false,
                builder_id: None,
                builder_ids: None,
            },
            pub_key: builder_pub_key.clone(),
        }];

        cache.update_builder_infos(&builder_infos).await.unwrap();

        let slot = 42;
        let block_hash = B256::random();
        let time = 1616237123000u64;

        cache.save_pending_block_header(slot, &builder_pub_key, &block_hash, time).await.unwrap();

        cache.save_pending_block_payload(slot, &builder_pub_key, &block_hash, time).await.unwrap();

        cache
            .save_pending_block_payload(slot, &builder_pub_key, &block_hash, 1716237123000u64)
            .await
            .unwrap();

        let pending_blocks = cache.get_pending_blocks().await.unwrap();
        assert_eq!(pending_blocks.len(), 1);

        for i in pending_blocks {
            assert_eq!(i.slot, slot);
            assert_eq!(i.block_hash, block_hash);
            assert_eq!(i.builder_pubkey, builder_pub_key);
            assert_eq!(i.header_receive_ms, Some(time));
            assert_eq!(i.payload_receive_ms, Some(time));
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_kill_switch() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let result = cache.kill_switch_enabled().await.unwrap();
        assert!(!result, "Kill switch should be disabled by default");

        cache.enable_kill_switch().await.unwrap();

        let result = cache.kill_switch_enabled().await.unwrap();
        assert!(result, "Kill switch should be enabled");

        cache.disable_kill_switch().await.unwrap();

        let result = cache.kill_switch_enabled().await.unwrap();
        assert!(!result, "Kill switch should be disabled");
    }
}
