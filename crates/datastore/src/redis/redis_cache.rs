#![allow(dependency_on_unit_never_type_fallback)] // TODO: temp fix , needs to be fixed before upading to 2024 edition

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use alloy_primitives::B256;
use async_trait::async_trait;
use deadpool_redis::{Config, Connection, CreatePoolError, Pool, Runtime};
use futures_util::StreamExt;
use helix_beacon::types::{HeadEventData, PayloadAttributesEvent};
use helix_common::{
    api::builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
    bid_sorter::BidSorterMessage,
    metrics::RedisMetricRecord,
    BuilderInfo, ProposerInfo,
};
use helix_database::types::BuilderInfoDocument;
use helix_types::{
    maybe_upgrade_execution_payload, BidTrace, BlockMergingData, BlsPublicKey, ForkName,
    PayloadAndBlobs, PayloadAndBlobsRef,
};
use moka::sync::Cache;
use parking_lot::RwLock;
use redis::{AsyncCommands, Msg, RedisResult, Script, Value};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::from_str;
use tokio::sync::broadcast;
use tracing::{error, info, instrument, warn};

use crate::{
    error::AuctioneerError,
    redis::{
        error::RedisCacheError,
        utils::{
            get_cache_bid_trace_key, get_cache_block_merging_data_key, get_execution_payload_key,
        },
    },
    types::keys::{
        BUILDER_INFO_KEY, CURRENT_INCLUSION_LIST_KEY, HOUSEKEEPER_LOCK_KEY, KILL_SWITCH,
        LAST_HASH_DELIVERED_KEY, LAST_SLOT_DELIVERED_KEY, PAYLOAD_ADDRESS_KEY,
        PRIMEV_PROPOSERS_KEY, PROPOSER_WHITELIST_KEY,
    },
    Auctioneer,
};

const BID_CACHE_EXPIRY_S: usize = 45;
const INCLUSION_LIST_EXPIRY_S: usize = 45;
const PAYLOAD_ADDRESS_EXPIRY_S: usize = 24;
const HOUSEKEEPER_LOCK_EXPIRY_MS: usize = 45_000;

const CURRENT_INCLUSION_LIST_CHANNEL: &str = "inclusion_list_updates";
const SEEN_BLOCK_HASHES_CHANNEL: &str = "seen_block_hashes_updates";
const LAST_SLOT_DELIVERED_CHANNEL: &str = "last_slot_delivered_updates";
const LAST_HASH_DELIVERED_CHANNEL: &str = "last_hash_delivered_updates";
const BUILDER_LAST_BID_RECEIVED_AT_CHANNEL: &str = "builder_last_bid_received_at_updates";
const BUILDER_LAST_BID_RECEIVED_AT_DELETED_CHANNEL: &str =
    "builder_last_bid_received_at_deleted_updates";
const BUILDER_INFO_CHANNEL: &str = "builder_info_channel";
const EXECUTION_PAYLOAD_CHANNEL: &str = "execution_payload_updates";
const PAYLOAD_ADDRESS_CHANNEL: &str = "payload_address_updates";
const PROPOSER_WHITELIST_CHANNEL: &str = "proposer_whitelist_updates";
const PROPOSER_WHITELIST_DELETED_CHANNEL: &str = "proposer_whitelist_deleted_updates";

const RENEW_SCRIPT: &str = r#"
if redis.call('get', KEYS[1]) == ARGV[1] then
    return redis.call('set', KEYS[1], ARGV[1], 'PX', ARGV[2], 'XX')
end
return nil
"#;

#[derive(Clone)]
pub struct RedisCache {
    pool: Pool,

    inclusion_list: broadcast::Sender<InclusionListWithKey>,
    payload_attributes: broadcast::Sender<PayloadAttributesEvent>,
    head_event: broadcast::Sender<HeadEventData>,

    seen_block_hashes: Cache<B256, ()>,
    last_delivered_slot: Arc<AtomicU64>,
    last_delivered_hash: Arc<RwLock<Option<B256>>>,
    builder_latest_payload_received_at: Cache<String, HashMap<String, u64>>,
    builder_info_cache: Cache<String, HashMap<String, BuilderInfo>>,
    trusted_proposers: Cache<String, HashMap<String, ProposerInfo>>,
    execution_payload_cache: Cache<String, PayloadAndBlobs>,
    payload_address_cache: Cache<String, (BlsPublicKey, Vec<u8>)>,

    sorter_tx: crossbeam_channel::Sender<BidSorterMessage>,
}

#[allow(dead_code)]
impl RedisCache {
    pub async fn new(
        conn_str: &str,
        builder_infos: Vec<BuilderInfoDocument>,
        sorter_tx: crossbeam_channel::Sender<BidSorterMessage>,
    ) -> Result<Self, CreatePoolError> {
        let mut cfg = Config::from_url(conn_str);
        let mut pool_config = deadpool_redis::PoolConfig::default();
        pool_config.max_size += 12; // Increase max size to accommodate listeners
        cfg.pool = Some(pool_config);
        let pool = cfg.create_pool(Some(Runtime::Tokio1))?;

        let (inclusion_list, mut il_recv) = broadcast::channel(1);
        let (payload_attributes, mut pa_recv) = broadcast::channel(1000);
        let (head_event, mut he_recv) = broadcast::channel(1000);

        // ensure at least one subscriber is running
        tokio::spawn(async move { while let Ok(_message) = il_recv.recv().await {} });
        tokio::spawn(async move { while let Ok(_message) = pa_recv.recv().await {} });
        tokio::spawn(async move { while let Ok(_message) = he_recv.recv().await {} });

        let seen_block_hashes =
            Cache::builder().time_to_idle(Duration::from_secs(45)).max_capacity(10_000).build();
        let builder_latest_payload_received_at =
            Cache::builder().time_to_idle(Duration::from_secs(45)).max_capacity(10_000).build();
        let builder_info_cache =
            Cache::builder().time_to_idle(Duration::from_secs(300)).max_capacity(10_000).build();
        let last_delivered_slot = Arc::new(AtomicU64::new(0));
        let last_delivered_hash = Arc::new(RwLock::new(None));
        let execution_payload_cache =
            Cache::builder().time_to_idle(Duration::from_secs(45)).max_capacity(10_000).build();

        let trusted_proposers = Cache::builder().max_capacity(200_000).build();

        let payload_address_cache = Cache::builder()
            .time_to_idle(Duration::from_secs(PAYLOAD_ADDRESS_EXPIRY_S as u64))
            .max_capacity(10_000)
            .build();

        let cache = Self {
            pool,
            inclusion_list,
            payload_attributes,
            head_event,
            seen_block_hashes,
            builder_latest_payload_received_at,
            last_delivered_slot,
            last_delivered_hash,
            builder_info_cache,
            trusted_proposers,
            execution_payload_cache,
            payload_address_cache,
            sorter_tx,
        };

        // Load in builder info
        if let Err(err) = cache.update_builder_infos(&builder_infos).await {
            error!(err=%err, "Failed to initialise builder info")
        }

        Ok(cache)
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

    pub async fn start_execution_payload_listener(&self) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe(EXECUTION_PAYLOAD_CHANNEL).await?;

        let mut message_stream = pubsub.on_message();
        let mut conn = self.pool.get().await?;

        while let Some(msg) = message_stream.next().await {
            let Ok((key, execution_payload)) = Self::process_pubsub_update(msg, &mut conn).await
            else {
                continue;
            };

            self.execution_payload_cache.insert(key, execution_payload);
        }

        Ok(())
    }

    pub async fn start_payload_address_listener(&self) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe(PAYLOAD_ADDRESS_CHANNEL).await?;

        let mut message_stream = pubsub.on_message();
        let mut conn = self.pool.get().await?;

        while let Some(msg) = message_stream.next().await {
            let Ok((key, payload_address)) = Self::process_pubsub_update(msg, &mut conn).await
            else {
                continue;
            };

            self.payload_address_cache.insert(key, payload_address);
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
                if let Ok(block_hash) = from_str::<B256>(&key) {
                    self.seen_block_hashes.insert(block_hash, ());
                }
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

    pub async fn start_last_hash_delivered_listener(&self) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe(LAST_HASH_DELIVERED_CHANNEL).await?;

        let mut message_stream = pubsub.on_message();

        while let Some(message) = message_stream.next().await {
            let payload = message.get_payload::<String>().ok();
            if let Some(key) = payload {
                if let Ok(block_hash) = from_str::<B256>(&key) {
                    self.last_delivered_hash.write().replace(block_hash);
                }
            }
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
                if let Ok((key, builder_pub_key, received_at)) =
                    from_str::<(String, String, u64)>(&payload_str)
                {
                    self.builder_latest_payload_received_at
                        .get_with(key, HashMap::new)
                        .insert(builder_pub_key, received_at);
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
            if let Some(payload_str) = payload {
                if let Ok((key, builder_pub_key)) = from_str::<(String, String)>(&payload_str) {
                    self.builder_latest_payload_received_at
                        .get_with(key, HashMap::new)
                        .remove(&builder_pub_key);
                }
            }
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

            self.builder_info_cache
                .get_with(BUILDER_INFO_KEY.to_string(), HashMap::new)
                .insert(field, builder_info.clone());
        }

        Ok(())
    }

    pub async fn start_trusted_proposers_listener(&self) -> Result<(), RedisCacheError> {
        self.load_trusted_proposers().await?;

        let conn = self.pool.get().await?;

        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe(PROPOSER_WHITELIST_CHANNEL).await?;

        let mut message_stream = pubsub.on_message();

        let mut conn = self.pool.get().await?;

        while let Some(message) = message_stream.next().await {
            let Ok((field, proposer_info)) = Self::process_pubsub_update_hget::<ProposerInfo>(
                PROPOSER_WHITELIST_KEY,
                message,
                &mut conn,
            )
            .await
            else {
                continue;
            };

            self.trusted_proposers
                .get_with(PROPOSER_WHITELIST_KEY.to_string(), HashMap::new)
                .insert(field, proposer_info.clone());
        }

        Ok(())
    }

    async fn load_trusted_proposers(&self) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let entries: HashMap<String, Vec<u8>> = conn.hgetall(PROPOSER_WHITELIST_KEY).await?;
        if entries.is_empty() {
            return Ok(());
        }

        let mut deserialized_entries = HashMap::with_capacity(entries.len());
        for (key, value) in entries.into_iter() {
            let deserialized_value = serde_json::from_slice(&value)?;
            deserialized_entries.insert(key, deserialized_value);
        }

        self.trusted_proposers.insert(PROPOSER_WHITELIST_KEY.to_string(), deserialized_entries);

        Ok(())
    }

    pub async fn start_proposer_whitelist_deleted_listener(&self) -> Result<(), RedisCacheError> {
        let conn = self.pool.get().await?;
        let mut pubsub = deadpool_redis::Connection::take(conn).into_pubsub();
        pubsub.subscribe(PROPOSER_WHITELIST_DELETED_CHANNEL).await?;

        let mut message_stream = pubsub.on_message();

        while let Some(message) = message_stream.next().await {
            let payload = message.get_payload::<String>().ok();
            if let Some(key) = payload {
                self.trusted_proposers
                    .get_with(PROPOSER_WHITELIST_KEY.to_string(), HashMap::new)
                    .remove(&key);
            }
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

    async fn get_with_cache<T: Clone + Send + Sync + serde::de::DeserializeOwned + 'static>(
        &self,
        key: &str,
        cache: &Cache<String, T>,
    ) -> Result<Option<T>, RedisCacheError> {
        if let Some(cached) = cache.get(key) {
            return Ok(Some(cached));
        }

        let value = self.get::<T>(key).await?;
        if let Some(ref val) = value {
            cache.insert(key.to_string(), val.clone());
        }

        Ok(value)
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

    async fn hget_with_cache<T: Clone + Send + Sync + DeserializeOwned + 'static>(
        &self,
        key: &str,
        field: &str,
        cache: &Cache<String, HashMap<String, T>>,
    ) -> Result<Option<T>, RedisCacheError> {
        if let Some(cached_map) = cache.get(key) {
            if let Some(cached_value) = cached_map.get(field) {
                return Ok(Some(cached_value.clone()));
            }
        }

        let value = self.hget::<T>(key, field).await?;
        if let Some(ref val) = value {
            cache
                .get_with(key.to_string(), || HashMap::new())
                .insert(field.to_string(), val.clone());
        }

        Ok(value)
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

    async fn hgetall_with_cache<V: Clone + Send + Sync + DeserializeOwned + 'static>(
        &self,
        key: &str,
        cache: &Cache<String, HashMap<String, V>>,
    ) -> Result<Option<HashMap<String, V>>, RedisCacheError> {
        if let Some(cached) = cache.get(key) {
            return Ok(Some(cached.clone()));
        }

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

        if cache.get(key).is_none() {
            cache.insert(key.to_string(), deserialized_entries.clone());
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

    async fn set_with_cache<T: Clone + Serialize + Send + Sync + 'static>(
        &self,
        key: &str,
        value: &T,
        cache: &Cache<String, T>,
        expiry: Option<usize>,
    ) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let str_val = serde_json::to_string(value)?;
        cache.insert(key.to_string(), value.clone());

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

    async fn hset_with_cache<T: Clone + Serialize + Send + Sync + 'static>(
        &self,
        key: &str,
        field: &str,
        value: &T,
        cache: &Cache<String, HashMap<String, T>>,
    ) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let str_val = serde_json::to_string(value)?;
        cache.get_with(key.to_string(), || HashMap::new()).insert(field.to_string(), value.clone());
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

    async fn hdel_with_cache<T: Clone + Send + Sync + 'static>(
        &self,
        key: &str,
        field: &str,
        cache: &Cache<String, HashMap<String, T>>,
    ) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        cache.get_with(key.to_string(), || HashMap::new()).remove(field);
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

    async fn get_last_hash_delivered(&self) -> Result<Option<B256>, RedisCacheError> {
        let maybe = {
            let guard = self.last_delivered_hash.read();
            *guard
        };

        if maybe.is_some() {
            return Ok(maybe);
        }

        self.get(LAST_HASH_DELIVERED_KEY).await
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
        self.last_delivered_hash.write().replace(*hash);

        // Clone what is needed for the spawned task
        let pool = self.pool.clone();
        let slot_value = match serde_json::to_string(&slot) {
            Ok(value) => value,
            Err(err) => {
                error!(%err, "Failed to serialize slot");
                record.record_success();
                return Ok(());
            }
        };
        let hash_string = format!("{hash:?}");
        let hash_value = match serde_json::to_string(&hash_string) {
            Ok(value) => value,
            Err(err) => {
                error!(%err, "Failed to serialize hash");
                record.record_success();
                return Ok(());
            }
        };
        let slot_value_clone = slot_value.clone();

        tokio::spawn(async move {
            let mut conn = match pool.get().await {
                Ok(conn) => conn,
                Err(err) => {
                    error!(%err, "Failed to get redis connection");
                    return;
                }
            };

            let mut pipe = redis::pipe();
            pipe.atomic()
                .cmd("SET")
                .arg(LAST_SLOT_DELIVERED_KEY)
                .arg(&slot_value)
                .ignore()
                .cmd("SET")
                .arg(LAST_HASH_DELIVERED_KEY)
                .arg(&hash_value)
                .ignore()
                .cmd("PUBLISH")
                .arg(LAST_SLOT_DELIVERED_CHANNEL)
                .arg(&slot_value_clone)
                .ignore()
                .cmd("PUBLISH")
                .arg(LAST_HASH_DELIVERED_CHANNEL)
                .arg(&hash_value)
                .ignore();

            if let Err(err) = pipe
                .query_async::<deadpool_redis::Connection, ()>(&mut conn)
                .await
                .map_err(RedisCacheError::from)
            {
                error!(%err, "Failed to execute redis pipeline");
            }
        });

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn save_execution_payload<'a>(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &B256,
        execution_payload: PayloadAndBlobsRef<'a>,
    ) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("save_execution_payload");

        let key = get_execution_payload_key(slot, proposer_pub_key, block_hash);
        self.set_with_cache(
            &key,
            &execution_payload.to_owned(),
            &self.execution_payload_cache,
            Some(BID_CACHE_EXPIRY_S),
        )
        .await?;

        // self.publish(EXECUTION_PAYLOAD_CHANNEL, &key).await?;

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
        let execution_payload = self
            .get_with_cache::<PayloadAndBlobs>(&key, &self.execution_payload_cache)
            .await?
            .map(|p| PayloadAndBlobs {
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
    async fn get_block_merging_data(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &B256,
    ) -> Result<Option<BlockMergingData>, AuctioneerError> {
        let mut record = RedisMetricRecord::new("get_block_merging_data");

        let key = get_cache_block_merging_data_key(slot, proposer_pub_key, block_hash);
        let bid_trace = self.get(&key).await?;

        record.record_success();
        Ok(bid_trace)
    }

    #[instrument(skip_all)]
    async fn save_block_merging_data(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &B256,
        merging_data: &BlockMergingData,
    ) -> Result<(), AuctioneerError> {
        let mut record = RedisMetricRecord::new("save_block_merging_data");

        let key = get_cache_block_merging_data_key(slot, proposer_pub_key, block_hash);
        self.set(&key, merging_data, Some(BID_CACHE_EXPIRY_S)).await?;

        record.record_success();
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_builder_info(
        &self,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<BuilderInfo, AuctioneerError> {
        let mut record = RedisMetricRecord::new("get_builder_info");

        let builder_info = self
            .hget_with_cache(
                BUILDER_INFO_KEY,
                &format!("{builder_pub_key:?}"),
                &self.builder_info_cache,
            )
            .await?
            .ok_or(AuctioneerError::BuilderNotFound { pub_key: builder_pub_key.clone() })?;

        record.record_success();
        Ok(builder_info)
    }

    #[instrument(skip_all)]
    async fn demote_builder(&self, builder_pub_key: &BlsPublicKey) -> Result<(), AuctioneerError> {
        let _ = self.sorter_tx.try_send(BidSorterMessage::Demotion(builder_pub_key.clone()));

        let mut record = RedisMetricRecord::new("demote_builder");
        let mut builder_info = self.get_builder_info(builder_pub_key).await?;
        if !builder_info.is_optimistic {
            return Ok(());
        }
        builder_info.is_optimistic = false;
        builder_info.is_optimistic_for_regional_filtering = false;
        self.hset_with_cache(
            BUILDER_INFO_KEY,
            &format!("{builder_pub_key:?}"),
            &builder_info,
            &self.builder_info_cache,
        )
        .await?;
        self.publish(BUILDER_INFO_CHANNEL, &format!("{builder_pub_key:?}")).await?;

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
        let redis_builder_infos: HashMap<String, BuilderInfo> = self
            .hgetall_with_cache(BUILDER_INFO_KEY, &self.builder_info_cache)
            .await?
            .unwrap_or_default();

        // Update Redis value if the builder info has changed or it's a new builder
        for builder_info in builder_infos {
            let builder_pub_key_str = format!("{:?}", builder_info.pub_key);
            if let Some(redis_builder_info) = redis_builder_infos.get(&builder_pub_key_str) {
                if builder_info.builder_info != *redis_builder_info {
                    self.hset_with_cache(
                        BUILDER_INFO_KEY,
                        &builder_pub_key_str,
                        &builder_info.builder_info,
                        &self.builder_info_cache,
                    )
                    .await?;
                    self.publish(BUILDER_INFO_CHANNEL, &builder_pub_key_str).await?;
                    info!(
                        pubkey = %builder_info.pub_key,
                        is_optimistic = builder_info.builder_info.is_optimistic,
                        "updated builder info",
                    );
                }
            } else {
                self.hset_with_cache(
                    BUILDER_INFO_KEY,
                    &builder_pub_key_str,
                    &builder_info.builder_info,
                    &self.builder_info_cache,
                )
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
    async fn seen_or_insert_block_hash(&self, block_hash: &B256) -> Result<bool, AuctioneerError> {
        let mut record = RedisMetricRecord::new("seen_or_insert_block_hash");

        if self.seen_block_hashes.contains_key(block_hash) {
            record.record_success();
            return Ok(true);
        }

        self.seen_block_hashes.insert(*block_hash, ());
        self.publish_json(SEEN_BLOCK_HASHES_CHANNEL, &block_hash).await?;
        record.record_success();
        Ok(false)
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

        let proposer_info: Option<HashMap<String, ProposerInfo>> =
            self.hgetall_with_cache(PROPOSER_WHITELIST_KEY, &self.trusted_proposers).await?;

        // add or update proposers
        for proposer in proposer_whitelist {
            // If the proposer is already in the cache, we can skip the hset operation
            if let Some(existing_proposer) =
                proposer_info.as_ref().and_then(|info| info.get(&format!("{:?}", proposer.pubkey)))
            {
                if existing_proposer == &proposer {
                    continue; // No change needed
                }
            }
            let key_str = format!("{:?}", proposer.pubkey);
            self.hset_with_cache(
                PROPOSER_WHITELIST_KEY,
                &key_str,
                &proposer,
                &self.trusted_proposers,
            )
            .await?;
            self.publish(PROPOSER_WHITELIST_CHANNEL, &key_str).await?;
        }

        // remove any proposers that are no longer in the list
        if let Some(proposer_info) = proposer_info {
            for key in proposer_info.keys() {
                if !proposer_keys.contains(key) {
                    self.hdel_with_cache(PROPOSER_WHITELIST_KEY, key, &self.trusted_proposers)
                        .await?;
                    self.publish(PROPOSER_WHITELIST_DELETED_CHANNEL, key).await?;
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

        if let Some(cached_map) = self.trusted_proposers.get(PROPOSER_WHITELIST_KEY) {
            record.record_success();
            return Ok(cached_map.contains_key(&key_str));
        }

        record.record_success();
        Ok(false)
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
        // TODO: This should use get_with_cache
        // but currently V3 isn't really used yet in production
        // so that would always fall back to redis and delay payload processing
        // ultimately causing a missed slot
        // when V3 is used in production, we should switch to get_with_cache
        let payload_address: Option<(BlsPublicKey, Vec<u8>)> = self.payload_address_cache.get(&key);
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
        self.set_with_cache(
            &key,
            &(builder_pub_key.clone(), payload_socket_address),
            &self.payload_address_cache,
            Some(PAYLOAD_ADDRESS_EXPIRY_S),
        )
        .await?;
        self.publish(PAYLOAD_ADDRESS_CHANNEL, &key).await?;
        record.record_success();
        Ok(())
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
        let publish_fut = self.publish_json("head_event_channel", head_event);
        let timeout = Duration::from_secs(3);

        if tokio::time::timeout(timeout, publish_fut).await.is_err() {
            error!("Failed to publish head event within timeout");
        }
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
        let publish_fut = self.publish_json("payload_attributes_channel", &payload_attributes);
        let timeout = Duration::from_secs(3);

        if tokio::time::timeout(timeout, publish_fut).await.is_err() {
            error!("Failed to publish payload attributes within timeout");
        }
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

#[cfg(test)]
mod tests {

    use alloy_primitives::U256;
    use helix_types::{
        get_fixed_pubkey, BlobsBundle, ExecutionPayloadElectra, ExecutionPayloadRef, ForkName,
        TestRandomSeed,
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
    async fn test_get_and_set_object() {
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
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
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
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
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
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
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
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
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
        cache.clear_cache().await.unwrap();

        let value = "test_value";
        let set_result = cache.rpush("test_rpush_key", &value).await;
        assert!(set_result.is_ok(), "Failed to rpush");
    }

    #[tokio::test]
    #[serial]
    async fn test_clear_key() {
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
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

    /// #######################################################################
    /// ########################### Auctioneer tests ##########################
    /// #######################################################################

    #[tokio::test]
    #[serial]
    async fn test_get_and_check_last_slot_and_hash_delivered() {
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
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
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
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
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
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
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
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
    async fn test_get_and_save_execution_payload() {
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let proposer_pub_key = BlsPublicKey::test_random();
        let block_hash = B256::test_random();

        let payload = ExecutionPayloadElectra { gas_limit: 999, ..Default::default() };
        let blobs_bundle = BlobsBundle::default();
        let versioned_execution_payload = PayloadAndBlobsRef {
            execution_payload: ExecutionPayloadRef::Electra(&payload),
            blobs_bundle: &blobs_bundle,
        };

        // Save the execution payload
        let save_result = cache
            .save_execution_payload(
                slot,
                &proposer_pub_key,
                &block_hash,
                versioned_execution_payload,
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
    async fn test_get_builder_info() {
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
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
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
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
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
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
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
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
    async fn test_seen_or_insert_block_hash() {
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
        let cloned_cache = cache.clone();
        tokio::spawn(async move {
            cloned_cache.start_seen_block_hashes_listener().await.unwrap();
        });
        cache.clear_cache().await.unwrap();

        let _slot = 42;
        let block_hash = B256::random();
        let _pubkey = get_fixed_pubkey(0);

        // Test: Check if block hash has been seen before (should be false initially)
        let seen_result = cache.seen_or_insert_block_hash(&block_hash).await;
        assert!(seen_result.is_ok(), "Failed to check if block hash was seen");
        assert!(!seen_result.unwrap(), "Block hash was incorrectly seen before");

        // Test: Insert the block hash and check again (should be true after insert)
        let seen_result_again = cache.seen_or_insert_block_hash(&block_hash).await;
        assert!(seen_result_again.is_ok(), "Failed to check if block hash was seen after insert");
        assert!(seen_result_again.unwrap(), "Block hash was not seen after insert");

        // Test: Add a different new block hash (should be false initially)
        let block_hash_2 = B256::random();
        let seen_result = cache.seen_or_insert_block_hash(&block_hash_2).await;
        assert!(seen_result.is_ok(), "Failed to check if block hash was seen");
        assert!(!seen_result.unwrap(), "Block hash was incorrectly seen before");

        // Test: Insert the original block hash again, ensure it wasn't overwritten (should be true
        // after insert)
        let seen_result_again = cache.seen_or_insert_block_hash(&block_hash).await;
        assert!(seen_result_again.is_ok(), "Failed to check if block hash was seen after insert");
        assert!(seen_result_again.unwrap(), "Block hash was not seen after insert");
    }

    #[tokio::test]
    #[serial]
    async fn test_can_aquire_lock() {
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
        cache.clear_cache().await.unwrap();
        assert!(cache.try_acquire_or_renew_leadership("leader").await)
    }

    #[tokio::test]
    #[serial]
    async fn test_others_cant_aquire_lock_if_held() {
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
        cache.clear_cache().await.unwrap();
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
        assert!(!cache.try_acquire_or_renew_leadership("others").await);
    }

    #[tokio::test]
    #[serial]
    async fn test_can_renew_lock() {
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
        cache.clear_cache().await.unwrap();
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
    }

    #[tokio::test]
    #[serial]
    async fn test_others_cannot_renew() {
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
        cache.clear_cache().await.unwrap();
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
        assert!(!cache.try_acquire_or_renew_leadership("others").await);
    }

    #[tokio::test]
    #[serial]
    async fn test_kill_switch() {
        let cache =
            RedisCache::new("redis://127.0.0.1/", Vec::new(), crossbeam_channel::bounded(1).0)
                .await
                .unwrap();
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
