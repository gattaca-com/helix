use std::collections::HashMap;

use async_trait::async_trait;
use deadpool_redis::{Config, CreatePoolError, Pool, Runtime};
use ethereum_consensus::{
    primitives::{BlsPublicKey, Hash32},
    ssz::{self, prelude::*},
};
use futures_util::TryStreamExt;
use helix_common::{
    api::builder_api::TopBidUpdate,
    bid_submission::{v2::header_submission::SignedHeaderSubmission, BidSubmission},
    pending_block::PendingBlock,
    versioned_payload::PayloadAndBlobs,
    ProposerInfo,
};
use redis::{AsyncCommands, RedisResult, Script, Value};
use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::broadcast;
use tracing::error;

use helix_common::{
    bid_submission::{BidTrace, SignedBidSubmission},
    eth::SignedBuilderBid,
    signing::RelaySigningContext,
    BuilderInfo,
};
use helix_database::types::BuilderInfoDocument;

use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};

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
            BUILDER_INFO_KEY, HOUSEKEEPER_LOCK_KEY, KILL_SWITCH, LAST_HASH_DELIVERED_KEY,
            LAST_SLOT_DELIVERED_KEY, PRIMEV_PROPOSERS_KEY, PROPOSER_WHITELIST_KEY,
        },
        signed_builder_bid_wrapper::SignedBuilderBidWrapper,
        SaveBidAndUpdateTopBidResponse,
    },
    Auctioneer,
};

use super::utils::{
    get_hash_from_hex, get_header_tx_root_key, get_pending_block_builder_block_hash_key,
    get_pending_block_builder_key, get_pubkey_from_hex,
};

const BID_CACHE_EXPIRY_S: usize = 45;
const PENDING_BLOCK_EXPIRY_S: usize = 45;
const HOUSEKEEPER_LOCK_EXPIRY_MS: usize = 45_000;

const BEST_BIDS_CHANNEL: &str = "best_bids";

const RENEW_SCRIPT: &str = r#"
if redis.call('get', KEYS[1]) == ARGV[1] then
    return redis.call('set', KEYS[1], ARGV[1], 'PX', ARGV[2], 'XX')
end
return nil
"#;

#[derive(Clone)]
pub struct RedisCache {
    pool: Pool,
    tx: broadcast::Sender<Vec<u8>>,
}

impl RedisCache {
    pub async fn new(
        conn_str: &str,
        builder_infos: Vec<BuilderInfoDocument>,
    ) -> Result<Self, CreatePoolError> {
        let cfg = Config::from_url(conn_str);
        let pool = cfg.create_pool(Some(Runtime::Tokio1))?;
        let (tx, mut rx) = broadcast::channel(1000);

        // ensure at least one subscriber is running
        tokio::spawn(async move { while let Ok(_message) = rx.recv().await {} });

        let cache = Self { pool, tx };

        // Load in builder info
        if let Err(err) = cache.update_builder_infos(builder_infos).await {
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
            let payload: String = match message.get_payload() {
                Ok(payload) => payload,
                Err(err) => {
                    error!(err=%err, "Failed to get payload from message");
                    continue
                }
            };

            let data: String = match conn.get(payload).await {
                Ok(data) => data,
                Err(err) => {
                    error!(err=%err, "Failed to get data from redis");
                    continue
                }
            };
            let sig_bid: SignedBuilderBidWrapper = match serde_json::from_str(&data) {
                Ok(sig_bid) => sig_bid,
                Err(err) => {
                    error!(err=%err, "Failed to deserialize data");
                    continue
                }
            };

            let top_bid_update: TopBidUpdate = sig_bid.into();
            //ssz encode the top bid update
            let serialized = match ssz::prelude::serialize(&top_bid_update) {
                Ok(serialized) => serialized,
                Err(err) => {
                    error!(err=%err, "Failed to serialize top bid update");
                    continue
                }
            };

            if let Err(err) = self.tx.send(serialized) {
                error!(err=%err, "Failed to send top bid update");
                continue
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
            return Ok(None)
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

    async fn hset_multiple<T: Serialize>(
        &self,
        key: &str,
        entries: &[(&str, T)],
        expiry: usize,
    ) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let mut pipeline = redis::pipe();

        // Prepare a vector to hold serialized values
        let mut fields_and_values: Vec<(&str, String)> = Vec::new();

        // Iterate over the entries to serialize each value
        for (field, value) in entries {
            let str_val = serde_json::to_string(value)?;
            fields_and_values.push((field, str_val));
        }

        // Use hset_multiple for setting multiple fields at once
        pipeline.hset_multiple(key, &fields_and_values);
        pipeline.expire(key, expiry);

        Ok(pipeline.query_async(&mut conn).await?)
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
            })
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
        parent_hash: &Hash32,
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
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
        floor_value: U256,
    ) -> Result<(), RedisCacheError> {
        if builder_bids.is_empty() {
            return Ok(())
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
        self.set(&top_bid_value_key, &state.top_bid_value, Some(BID_CACHE_EXPIRY_S)).await?;

        self.publish(BEST_BIDS_CHANNEL, &top_bid_key).await?;

        state.was_top_bid_updated = state.prev_top_bid_value != state.top_bid_value;

        Ok(())
    }

    async fn get_last_hash_delivered(&self) -> Result<Option<Hash32>, RedisCacheError> {
        self.get(LAST_HASH_DELIVERED_KEY).await
    }

    /// 1) Update floor bid payload by copying from the best builder bid to floor bid.
    /// 2) Update floor bid value.
    async fn set_new_floor(
        &self,
        new_floor_value: U256,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<(), RedisCacheError> {
        let key_bid_source =
            get_latest_bid_by_builder_key(slot, parent_hash, proposer_pub_key, builder_pub_key);
        let key_floor_bid = get_floor_bid_key(slot, parent_hash, proposer_pub_key);
        self.copy(&key_bid_source, &key_floor_bid, Some(BID_CACHE_EXPIRY_S)).await?;

        let key_floor_bid_value = get_floor_bid_value_key(slot, parent_hash, proposer_pub_key);
        self.set(&key_floor_bid_value, &new_floor_value, Some(BID_CACHE_EXPIRY_S)).await
    }
}

#[async_trait]
impl Auctioneer for RedisCache {
    async fn get_last_slot_delivered(&self) -> Result<Option<u64>, AuctioneerError> {
        self.get(LAST_SLOT_DELIVERED_KEY).await.map_err(AuctioneerError::RedisError)
    }

    async fn check_and_set_last_slot_and_hash_delivered(
        &self,
        slot: u64,
        hash: &Hash32,
    ) -> Result<(), AuctioneerError> {
        let last_slot_delivered_res = self.get_last_slot_delivered().await?;

        if let Some(last_slot_delivered) = last_slot_delivered_res {
            if slot < last_slot_delivered {
                return Err(AuctioneerError::PastSlotAlreadyDelivered)
            }

            if slot == last_slot_delivered {
                let last_hash_delivered_res = self.get_last_hash_delivered().await?;

                match last_hash_delivered_res {
                    Some(last_hash_delivered) => {
                        if *hash != last_hash_delivered {
                            return Err(AuctioneerError::AnotherPayloadAlreadyDeliveredForSlot)
                        }
                    }
                    None => return Err(AuctioneerError::UnexpectedValueType),
                }
                return Ok(())
            }
        }

        let mut conn = self.pool.get().await.map_err(RedisCacheError::from)?;
        let mut pipe = redis::pipe();

        // Add the SET commands to the pipeline
        let slot_value = serde_json::to_string(&slot).map_err(RedisCacheError::from)?;
        let hash_value =
            serde_json::to_string(&format!("{hash:?}")).map_err(RedisCacheError::from)?;
        pipe.atomic()
            .cmd("SET")
            .arg(LAST_SLOT_DELIVERED_KEY)
            .arg(slot_value)
            .ignore()
            .cmd("SET")
            .arg(LAST_HASH_DELIVERED_KEY)
            .arg(hash_value)
            .ignore();

        Ok(pipe.query_async(&mut conn).await.map_err(RedisCacheError::from)?)
    }

    async fn get_best_bid(
        &self,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<SignedBuilderBid>, AuctioneerError> {
        let key = get_cache_get_header_response_key(slot, parent_hash, proposer_pub_key);
        let wrapped_bid: Option<SignedBuilderBidWrapper> = self.get(&key).await?;
        Ok(wrapped_bid.map(|wrapped_bid| wrapped_bid.bid))
    }

    async fn get_best_bids(
        &self,
    ) -> Box<dyn Stream<Item = Result<Vec<u8>, AuctioneerError>> + Send + Unpin> {
        let rx = self.tx.subscribe();
        let stream = BroadcastStream::new(rx).map_err(AuctioneerError::from);
        Box::new(stream)
    }

    async fn save_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &Hash32,
        execution_payload: &PayloadAndBlobs,
    ) -> Result<(), AuctioneerError> {
        let key = get_execution_payload_key(slot, proposer_pub_key, block_hash);
        Ok(self.set(&key, &execution_payload, Some(BID_CACHE_EXPIRY_S)).await?)
    }

    async fn get_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &Hash32,
    ) -> Result<Option<PayloadAndBlobs>, AuctioneerError> {
        let key = get_execution_payload_key(slot, proposer_pub_key, block_hash);
        Ok(self.get(&key).await?)
    }

    async fn get_bid_trace(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &Hash32,
    ) -> Result<Option<BidTrace>, AuctioneerError> {
        let key = get_cache_bid_trace_key(slot, proposer_pub_key, block_hash);
        Ok(self.get(&key).await?)
    }

    async fn save_bid_trace(&self, bid_trace: &BidTrace) -> Result<(), AuctioneerError> {
        let key = get_cache_bid_trace_key(
            bid_trace.slot,
            &bid_trace.proposer_public_key,
            &bid_trace.block_hash,
        );
        Ok(self.set(&key, &bid_trace, Some(BID_CACHE_EXPIRY_S)).await?)
    }

    async fn get_builder_latest_payload_received_at(
        &self,
        slot: u64,
        builder_pub_key: &BlsPublicKey,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<u64>, AuctioneerError> {
        let key = get_builder_latest_bid_time_key(slot, parent_hash, proposer_pub_key);
        Ok(self.hget(&key, &format!("{builder_pub_key:?}")).await?)
    }

    /// This function performs three operations:
    /// 1. Stores the full `SignedBuilderBid` object.
    /// 2. Stores the time at which this bid was received.
    /// 3. Stores the value of the bid.
    async fn save_builder_bid(
        &self,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
        builder_pub_key: &BlsPublicKey,
        received_at: u128,
        builder_bid: &SignedBuilderBid,
    ) -> Result<(), AuctioneerError> {
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
        let serialised_value =
            serde_json::to_string(&builder_bid.value()).map_err(RedisCacheError::from)?;

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

        Ok(pipe.query_async(&mut conn).await.map_err(RedisCacheError::from)?)
    }

    /// The `save_bid_and_update_top_bid` function performs several key operations:
    ///
    /// 1. It first fetches the latest bids for a particular slot, parent hash, and proposer.
    /// 2. It then updates the top bid based on these fetched bids and a given floor value.
    /// 3. It saves the current submission as a new bid.
    /// 4. Optionally, it updates the floor value if the submission value is above the floor.
    async fn save_bid_and_update_top_bid(
        &self,
        submission: &SignedBidSubmission,
        received_at: u128,
        cancellations_enabled: bool,
        floor_value: U256,
        state: &mut SaveBidAndUpdateTopBidResponse,
        signing_context: &RelaySigningContext,
    ) -> Result<Option<(SignedBuilderBid, PayloadAndBlobs)>, AuctioneerError> {
        // Exit early if cancellations aren't enabled and the bid is below the floor.
        let is_bid_above_floor = submission.bid_trace().value > floor_value;
        if !cancellations_enabled && !is_bid_above_floor {
            return Ok(None)
        }

        // Save the execution payload
        self.save_execution_payload(
            submission.slot(),
            submission.proposer_public_key(),
            submission.block_hash(),
            &submission.payload_and_blobs(),
        )
        .await?;
        state.set_latency_save_payload();

        // Sign builder bid with relay pubkey.
        let mut cloned_submission = (*submission).clone();
        let builder_bid = SignedBuilderBid::from_submission(
            &mut cloned_submission,
            signing_context.public_key.clone(),
            &signing_context.signing_key,
            &signing_context.context,
        )?;

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

        Ok(Some((builder_bid, cloned_submission.payload_and_blobs())))
    }

    async fn get_top_bid_value(
        &self,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<U256>, AuctioneerError> {
        let key = get_top_bid_value_key(slot, parent_hash, proposer_pub_key);
        Ok(self.get(&key).await?)
    }

    async fn get_builder_latest_value(
        &self,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<Option<U256>, AuctioneerError> {
        let key = get_builder_latest_bid_value_key(slot, parent_hash, proposer_pub_key);
        Ok(self.hget(&key, &format!("{builder_pub_key:?}")).await?)
    }

    async fn get_floor_bid_value(
        &self,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<U256>, AuctioneerError> {
        let key = get_floor_bid_value_key(slot, parent_hash, proposer_pub_key);
        Ok(self.get(&key).await?)
    }

    async fn delete_builder_bid(
        &self,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<(), AuctioneerError> {
        // Delete the value
        let key_latest_value =
            get_builder_latest_bid_value_key(slot, parent_hash, proposer_pub_key);
        self.hdel(&key_latest_value, &format!("{builder_pub_key:?}")).await?;

        // Delete the time
        let key_latest_time = get_builder_latest_bid_time_key(slot, parent_hash, proposer_pub_key);
        self.hdel(&key_latest_time, &format!("{builder_pub_key:?}")).await?;

        // Update bids now to determine current top bid
        let mut state = SaveBidAndUpdateTopBidResponse::default();

        let builder_bids = self.get_new_builder_bids(slot, parent_hash, proposer_pub_key).await?;
        let floor_value = self
            .get_floor_bid_value(slot, parent_hash, proposer_pub_key)
            .await?
            .unwrap_or(U256::ZERO);

        Ok(self
            .update_top_bid(
                &mut state,
                &builder_bids,
                slot,
                parent_hash,
                proposer_pub_key,
                floor_value,
            )
            .await?)
    }

    async fn get_builder_info(
        &self,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<BuilderInfo, AuctioneerError> {
        self.hget(BUILDER_INFO_KEY, &format!("{builder_pub_key:?}"))
            .await?
            .ok_or(AuctioneerError::BuilderNotFound { pub_key: builder_pub_key.clone() })
    }

    async fn demote_builder(&self, builder_pub_key: &BlsPublicKey) -> Result<(), AuctioneerError> {
        let mut builder_info = self.get_builder_info(builder_pub_key).await?;
        if !builder_info.is_optimistic {
            return Ok(())
        }
        builder_info.is_optimistic = false;
        Ok(self.hset(BUILDER_INFO_KEY, &format!("{builder_pub_key:?}"), &builder_info).await?)
    }

    async fn update_builder_infos(
        &self,
        builder_infos: Vec<BuilderInfoDocument>,
    ) -> Result<(), AuctioneerError> {
        if builder_infos.is_empty() {
            return Ok(())
        }

        // Fetch current builder info
        let redis_builder_infos: HashMap<String, BuilderInfo> =
            self.hgetall(BUILDER_INFO_KEY).await?.unwrap_or_default();

        // Update Redis value if the builder info has changed or it's a new builder
        for builder_info in builder_infos {
            let builder_pub_key_str = format!("{:?}", builder_info.pub_key);
            if let Some(redis_builder_info) = redis_builder_infos.get(&builder_pub_key_str) {
                if builder_info.builder_info != *redis_builder_info {
                    self.hset(BUILDER_INFO_KEY, &builder_pub_key_str, &builder_info.builder_info)
                        .await?;
                }
            } else {
                self.hset(BUILDER_INFO_KEY, &builder_pub_key_str, &builder_info.builder_info)
                    .await?;
            }
        }

        Ok(())
    }

    async fn seen_or_insert_block_hash(
        &self,
        block_hash: &Hash32,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<bool, AuctioneerError> {
        let key = get_seen_block_hashes_key(slot, parent_hash, proposer_pub_key);
        Ok(self.seen_or_add(&key, block_hash).await?)
    }

    async fn save_signed_builder_bid_and_update_top_bid(
        &self,
        builder_bid: &SignedBuilderBid,
        bid_trace: &BidTrace,
        received_at: u128,
        cancellations_enabled: bool,
        floor_value: U256,
        state: &mut SaveBidAndUpdateTopBidResponse,
    ) -> Result<(), AuctioneerError> {
        // Exit early if cancellations aren't enabled and the bid is below the floor.
        let is_bid_above_floor = builder_bid.value() > floor_value;
        if !cancellations_enabled && !is_bid_above_floor {
            return Ok(())
        }

        // Load the latest bids from all builders for the current slot, parent hash, and proposer
        let mut builder_bids = self
            .get_new_builder_bids(
                bid_trace.slot,
                &bid_trace.parent_hash,
                &bid_trace.proposer_public_key,
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
            &bid_trace.proposer_public_key,
            &bid_trace.builder_public_key,
            received_at,
            builder_bid,
        )
        .await?;
        state.was_bid_saved = true;
        builder_bids.insert(format!("{:?}", bid_trace.builder_public_key), builder_bid.value());
        state.set_latency_save_bid();

        // Save the bid trace
        self.save_bid_trace(bid_trace).await?;
        state.set_latency_save_trace();

        // Abort if the top bid hasn't changed
        // TODO: the floor may have raised but we will exit early here.
        state.top_bid_value = builder_bids.values().max().cloned().unwrap_or(U256::ZERO);
        if state.top_bid_value == state.prev_top_bid_value {
            return Ok(())
        }

        // Update the top bid
        self.update_top_bid(
            state,
            &builder_bids,
            bid_trace.slot,
            &bid_trace.parent_hash,
            &bid_trace.proposer_public_key,
            floor_value,
        )
        .await?;
        state.is_new_top_bid = builder_bid.value() == state.top_bid_value;
        state.set_latency_update_top_bid();

        // Handle floor value updates only if needed.
        // Only non-cancellable bids above the floor should set a new floor.
        if cancellations_enabled || !is_bid_above_floor {
            return Ok(())
        }
        self.set_new_floor(
            builder_bid.value(),
            bid_trace.slot,
            &bid_trace.parent_hash,
            &bid_trace.proposer_public_key,
            &bid_trace.builder_public_key,
        )
        .await?;
        state.set_latency_update_floor();

        Ok(())
    }

    async fn get_header_tx_root(
        &self,
        block_hash: &Hash32,
    ) -> Result<Option<Node>, AuctioneerError> {
        let key = get_header_tx_root_key(block_hash);
        Ok(self.get(&key).await?)
    }

    async fn save_header_submission_and_update_top_bid(
        &self,
        submission: &SignedHeaderSubmission,
        received_at: u128,
        cancellations_enabled: bool,
        floor_value: U256,
        state: &mut SaveBidAndUpdateTopBidResponse,
        signing_context: &RelaySigningContext,
    ) -> Result<Option<SignedBuilderBid>, AuctioneerError> {
        // Exit early if cancellations aren't enabled and the bid is below the floor.
        let is_bid_above_floor = submission.value() > floor_value;
        if !cancellations_enabled && !is_bid_above_floor {
            return Ok(None)
        }

        // Cache the transaction root for the header
        let key = get_header_tx_root_key(submission.block_hash());
        self.set(&key, &submission.transactions_root(), Some(24)).await?;

        // Sign builder bid with relay pubkey.
        let builder_bid = SignedBuilderBid::from_header_submission(
            submission,
            signing_context.public_key.clone(),
            &signing_context.signing_key,
            &signing_context.context,
        )?;

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

        Ok(Some(builder_bid))
    }

    async fn update_trusted_proposers(
        &self,
        proposer_whitelist: Vec<ProposerInfo>,
    ) -> Result<(), AuctioneerError> {
        // get keys
        let proposer_keys: Vec<String> =
            proposer_whitelist.iter().map(|proposer| format!("{:?}", proposer.pub_key)).collect();

        // add or update proposers
        for proposer in proposer_whitelist {
            let key_str = format!("{:?}", proposer.pub_key);
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

        Ok(())
    }

    async fn is_trusted_proposer(
        &self,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<bool, AuctioneerError> {
        let key_str = format!("{proposer_pub_key:?}");
        let proposer_info: Option<ProposerInfo> =
            self.hget(PROPOSER_WHITELIST_KEY, &key_str).await?;
        Ok(proposer_info.is_some())
    }

    async fn update_primev_proposers(
        &self,
        primev_proposers: &Vec<BlsPublicKey>,
    ) -> Result<(), AuctioneerError> {
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

        Ok(())
    }

    async fn is_primev_proposer(
        &self,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<bool, AuctioneerError> {
        let key_str = format!("{proposer_pub_key:?}");
        let proposer_info: Option<BlsPublicKey> = self.hget(PRIMEV_PROPOSERS_KEY, &key_str).await?;
        Ok(proposer_info.is_some())
    }

    async fn save_pending_block_header(
        &self,
        slot: u64,
        builder_pub_key: &BlsPublicKey,
        block_hash: &Hash32,
        timestamp_ms: u64,
    ) -> Result<(), AuctioneerError> {
        let builder_key = get_pending_block_builder_key(builder_pub_key);
        self.add(builder_key.as_str(), format!("{block_hash:?}")).await?;

        let key = get_pending_block_builder_block_hash_key(builder_pub_key, block_hash);
        let entries = vec![("slot", slot), ("header_received", timestamp_ms)];
        self.hset_multiple_not_exists(key.as_str(), &entries, PENDING_BLOCK_EXPIRY_S).await?;

        Ok(())
    }

    async fn save_pending_block_payload(
        &self,
        slot: u64,
        builder_pub_key: &BlsPublicKey,
        block_hash: &Hash32,
        timestamp_ms: u64,
    ) -> Result<(), AuctioneerError> {
        let builder_key = get_pending_block_builder_key(builder_pub_key);
        self.add(builder_key.as_str(), format!("{block_hash:?}")).await?;

        let key = get_pending_block_builder_block_hash_key(builder_pub_key, block_hash);
        let entries = vec![("slot", slot), ("payload_received", timestamp_ms)];
        self.hset_multiple_not_exists(key.as_str(), &entries, PENDING_BLOCK_EXPIRY_S).await?;

        Ok(())
    }

    async fn get_pending_blocks(&self) -> Result<Vec<PendingBlock>, AuctioneerError> {
        let mut pending_blocks: Vec<PendingBlock> = Vec::new();

        let redis_builder_infos: Option<HashMap<String, BuilderInfo>> =
            self.hgetall(BUILDER_INFO_KEY).await?;

        if redis_builder_infos.is_none() {
            return Ok(pending_blocks)
        }

        for (bulder_pub_key_str, builder_info) in redis_builder_infos.unwrap() {
            let mut expired: Vec<String> = Vec::new();

            let builder_pubkey = get_pubkey_from_hex(&bulder_pub_key_str)?;
            let builder_key: String = get_pending_block_builder_key(&builder_pubkey);

            if builder_info.is_optimistic {
                let block_hashes: Vec<String> = self.get_set_members(builder_key.as_str()).await?;
                let builder_pubkey_clone = builder_pubkey.clone();
                for block_hash_str in block_hashes {
                    let block_hash = get_hash_from_hex(&block_hash_str)?;
                    let key = get_pending_block_builder_block_hash_key(
                        &builder_pubkey_clone,
                        &block_hash,
                    );
                    let pending_block = self.hgetall_raw(key.as_str()).await?;

                    if pending_block.is_empty() {
                        expired.push(block_hash_str);
                        continue
                    }

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
                        builder_pubkey: builder_pubkey_clone.clone(),
                        header_receive_ms: header_received,
                        payload_receive_ms: payload_received,
                    };

                    pending_blocks.push(pending_block);
                }
            }

            if !expired.is_empty() {
                self.remove(builder_key.as_str(), expired).await?;
            }
        }
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
            return true
        }

        return self.set_lock(HOUSEKEEPER_LOCK_KEY, leader_id, HOUSEKEEPER_LOCK_EXPIRY_MS).await
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
}

fn get_top_bid(bid_values: &HashMap<String, U256>) -> Option<(String, U256)> {
    bid_values.iter().max_by_key(|&(_, value)| value).map(|(key, value)| (key.clone(), *value))
}

#[cfg(test)]
mod tests {

    use super::*;
    use ethereum_consensus::clock::get_current_unix_time_in_nanos;
    use helix_common::capella::{self, ExecutionPayloadHeader};

    use serde::{Deserialize, Serialize};

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

    #[tokio::test]
    async fn test_new() {
        let result = RedisCache::new("redis://127.0.0.1/", Vec::new()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
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
    async fn test_rpush() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let value = "test_value";
        let set_result = cache.rpush("test_rpush_key", &value).await;
        assert!(set_result.is_ok(), "Failed to rpush");
    }

    #[tokio::test]
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
    async fn test_get_new_builder_bids() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        // Current slot key
        let slot = 5_u64;
        let parent_hash = Hash32::default();
        let proposer_pub_key = BlsPublicKey::default();
        let key = get_builder_latest_bid_value_key(slot, &parent_hash, &proposer_pub_key);

        // Bid info
        let pub_key_1 = BlsPublicKey::try_from([1u8; 48].as_ref()).unwrap();
        let pub_key_2 = BlsPublicKey::try_from([2u8; 48].as_ref()).unwrap();
        let pub_key_3 = BlsPublicKey::try_from([3u8; 48].as_ref()).unwrap();

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
        let parent_hash = Hash32::default();
        let proposer_pub_key = BlsPublicKey::default();

        // Save prev best bid
        let prev_builder_pubkey = BlsPublicKey::try_from([23u8; 48].as_ref()).unwrap();
        let mut capella_builder_bid = helix_common::eth::capella::BuilderBid {
            header: ExecutionPayloadHeader::default(),
            value: U256::from(60),
            public_key: prev_builder_pubkey.clone(),
        };

        let prev_best_bid = SignedBuilderBid::Capella(capella::SignedBuilderBid {
            message: capella_builder_bid.clone(),
            ..Default::default()
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
        capella_builder_bid.value = higher_floor_value;
        let floor_bid = SignedBuilderBid::Capella(capella::SignedBuilderBid {
            message: capella_builder_bid.clone(),
            ..Default::default()
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
    async fn test_get_and_check_last_slot_and_hash_delivered() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let block_hash = Hash32::try_from([4u8; 32].as_ref()).unwrap();

        // Test: Save the last slot and hash delivered
        let set_result = cache.check_and_set_last_slot_and_hash_delivered(slot, &block_hash).await;
        assert!(set_result.is_ok(), "Saving last slot and hash delivered failed");

        // Test: Get the last slot delivered
        let get_result: Result<Option<u64>, _> = cache.get_last_slot_delivered().await;
        assert!(get_result.is_ok(), "Fetching last slot delivered failed");
        assert_eq!(get_result.unwrap().unwrap(), slot, "Slot value mismatch");
    }

    #[tokio::test]
    async fn test_set_past_slot() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let block_hash = Hash32::try_from([4u8; 32].as_ref()).unwrap();

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
    async fn test_set_same_slot_different_hash() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let block_hash1 = Hash32::try_from([4u8; 32].as_ref()).unwrap();
        let block_hash2 = Hash32::try_from([5u8; 32].as_ref()).unwrap();

        // Set initial slot and hash
        assert!(cache.check_and_set_last_slot_and_hash_delivered(slot, &block_hash1).await.is_ok());

        // Test: Set the same slot with a different hash
        let set_result = cache.check_and_set_last_slot_and_hash_delivered(slot, &block_hash2).await;
        assert!(matches!(set_result, Err(AuctioneerError::AnotherPayloadAlreadyDeliveredForSlot)));
    }

    #[tokio::test]
    async fn test_set_same_slot_no_hash() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let block_hash = Hash32::try_from([4u8; 32].as_ref()).unwrap();

        // Set just the slot but not the hash
        assert!(cache.set(LAST_SLOT_DELIVERED_KEY, &slot, None).await.is_ok());

        // Test: Set the same slot
        let set_result = cache.check_and_set_last_slot_and_hash_delivered(slot, &block_hash).await;
        assert!(matches!(set_result, Err(AuctioneerError::UnexpectedValueType)));
    }

    #[tokio::test]
    async fn test_get_and_set_best_bid() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let parent_hash = Hash32::default();
        let proposer_pub_key = BlsPublicKey::default();

        let mut capella_bid = capella::SignedBuilderBid::default();
        capella_bid.message.value = U256::from(1999);
        let best_bid = SignedBuilderBid::Capella(capella_bid);

        // Save the best bid
        let key = get_cache_get_header_response_key(slot, &parent_hash, &proposer_pub_key);
        let set_result = cache.set(&key, &best_bid, None).await;
        assert!(set_result.is_ok(), "Failed to set best bid in cache");

        // Test: Get the best bid
        let get_result: Result<Option<SignedBuilderBid>, _> =
            cache.get_best_bid(slot, &parent_hash, &proposer_pub_key).await;
        assert!(get_result.is_ok(), "Failed to get the best bid");
        assert!(get_result.as_ref().unwrap().is_some(), "Best bid was None");

        let fetched_builder_bid = get_result.unwrap().unwrap();
        assert_eq!(fetched_builder_bid.value(), U256::from(1999), "Best bid value mismatch");
    }

    #[tokio::test]
    async fn test_get_and_save_execution_payload() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let proposer_pub_key = BlsPublicKey::default();
        let block_hash = Hash32::default();

        let mut capella_payload = capella::ExecutionPayload::default();
        capella_payload.gas_limit = 999;
        let versioned_execution_payload = PayloadAndBlobs {
            execution_payload: ethereum_consensus::types::mainnet::ExecutionPayload::Capella(
                capella_payload,
            ),
            blobs_bundle: None,
        };

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
        let get_result: Result<Option<PayloadAndBlobs>, _> =
            cache.get_execution_payload(slot, &proposer_pub_key, &block_hash).await;
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
    async fn test_save_builder_bid_and_get_latest_payload_received_at() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        // Test data
        let slot = 1;
        let parent_hash = Hash32::default();
        let proposer_pub_key = BlsPublicKey::default();
        let builder_pub_key = BlsPublicKey::try_from([1u8; 48].as_ref()).unwrap();
        let received_at = 1616237123000u128;
        let value = U256::from(100);
        let block_hash = Hash32::try_from([4u8; 32].as_ref()).unwrap();

        let mut bid = capella::SignedBuilderBid {
            message: helix_common::eth::capella::BuilderBid { value, ..Default::default() },
            ..Default::default()
        };
        bid.message.header.block_hash = block_hash;
        let builder_bid = SignedBuilderBid::Capella(bid);

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
        let fetched_bid: Result<Option<SignedBuilderBid>, _> = cache.get(&key_latest_bid).await;
        assert!(fetched_bid.is_ok(), "Failed to fetch the latest bid");
        assert_eq!(
            fetched_bid.unwrap().unwrap().block_hash(),
            builder_bid.block_hash(),
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
        let parent_hash = Hash32::default();
        let proposer_pub_key = BlsPublicKey::default();
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
    async fn test_get_and_set_builder_latest_value() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let parent_hash = Hash32::default();
        let proposer_pub_key = BlsPublicKey::default();
        let builder_pub_key = BlsPublicKey::default();
        let latest_value = U256::from(100);

        // Set the latest value
        let key = get_builder_latest_bid_value_key(slot, &parent_hash, &proposer_pub_key);
        let set_result: Result<(), RedisCacheError> =
            cache.hset(&key, &format!("{builder_pub_key:?}"), &latest_value).await;
        assert!(set_result.is_ok(), "Failed to set the latest value");

        // Test: Get the latest value
        let get_result: Result<Option<U256>, _> = cache
            .get_builder_latest_value(slot, &parent_hash, &proposer_pub_key, &builder_pub_key)
            .await;
        assert!(get_result.is_ok(), "Failed to get the latest value");
        assert!(get_result.as_ref().unwrap().is_some(), "Latest value is None");
        assert_eq!(get_result.unwrap().unwrap(), latest_value, "Value mismatch");
    }

    #[tokio::test]
    async fn test_get_builder_info() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key = BlsPublicKey::default();
        let unknown_builder_pub_key = BlsPublicKey::try_from([23u8; 48].as_ref()).unwrap();

        let builder_info =
            BuilderInfo { collateral: U256::from(12), is_optimistic: true, builder_id: None };

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
    async fn test_get_trusted_proposers_and_update_trusted_proposers() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let is_trusted = cache.is_trusted_proposer(&BlsPublicKey::default()).await.unwrap();
        assert!(!is_trusted, "Failed to check trusted proposer");

        cache
            .update_trusted_proposers(vec![
                ProposerInfo { name: "test".to_string(), pub_key: BlsPublicKey::default() },
                ProposerInfo {
                    name: "test2".to_string(),
                    pub_key: BlsPublicKey::try_from([23u8; 48].as_ref()).unwrap(),
                },
            ])
            .await
            .unwrap();

        let is_trusted = cache.is_trusted_proposer(&BlsPublicKey::default()).await.unwrap();
        assert!(is_trusted, "Failed to check trusted proposer");

        let is_trusted = cache
            .is_trusted_proposer(&BlsPublicKey::try_from([23u8; 48].as_ref()).unwrap())
            .await
            .unwrap();
        assert!(is_trusted, "Failed to check trusted proposer");

        let is_trusted = cache
            .is_trusted_proposer(&BlsPublicKey::try_from([24u8; 48].as_ref()).unwrap())
            .await
            .unwrap();
        assert!(!is_trusted, "Failed to check trusted proposer");

        cache
            .update_trusted_proposers(vec![ProposerInfo {
                name: "test2".to_string(),
                pub_key: BlsPublicKey::try_from([25u8; 48].as_ref()).unwrap(),
            }])
            .await
            .unwrap();

        let is_trusted = cache.is_trusted_proposer(&BlsPublicKey::default()).await.unwrap();
        assert!(!is_trusted, "Failed to check trusted proposer");

        let is_trusted = cache
            .is_trusted_proposer(&BlsPublicKey::try_from([25u8; 48].as_ref()).unwrap())
            .await
            .unwrap();
        assert!(is_trusted, "Failed to check trusted proposer");
    }

    #[tokio::test]
    async fn test_demote_non_optimistic_builder() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key = BlsPublicKey::try_from([23u8; 48].as_ref()).unwrap();
        let builder_info =
            BuilderInfo { collateral: U256::from(12), is_optimistic: false, builder_id: None };

        // Set builder info in the cache
        let set_result =
            cache.hset(BUILDER_INFO_KEY, &format!("{builder_pub_key:?}"), &builder_info).await;
        assert!(set_result.is_ok(), "Failed to set builder info");

        // Test: Demote builder
        let result = cache.demote_builder(&builder_pub_key).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_demote_optimistic_builder() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key_optimistic = BlsPublicKey::try_from([11u8; 48].as_ref()).unwrap();
        let builder_info =
            BuilderInfo { collateral: U256::from(12), is_optimistic: true, builder_id: None };

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
    async fn test_delete_builder_bid() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        // Default vals
        let slot = 1;
        let parent_hash = Hash32::default();
        let proposer_pub_key = BlsPublicKey::default();
        let received_at = 12;

        // Save 2 builder bids. builder bid 1 > builder bid 2
        let builder_pub_key_1 = BlsPublicKey::try_from([1u8; 48].as_ref()).unwrap();
        let builder_bid_1 = SignedBuilderBid::Capella(capella::SignedBuilderBid {
            message: helix_common::eth::capella::BuilderBid {
                value: U256::from(100),
                ..Default::default()
            },
            ..Default::default()
        });

        let builder_pub_key_2 = BlsPublicKey::try_from([2u8; 48].as_ref()).unwrap();
        let builder_bid_2 = SignedBuilderBid::Capella(capella::SignedBuilderBid {
            message: helix_common::eth::capella::BuilderBid {
                value: U256::from(50),
                ..Default::default()
            },
            ..Default::default()
        });

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
        assert_eq!(top_bid.unwrap().unwrap().value(), U256::from(100), "Top bid mismatch");

        // Test: Delete best builder bid
        let delete_result = cache
            .delete_builder_bid(slot, &parent_hash, &proposer_pub_key, &builder_pub_key_1)
            .await;
        assert!(delete_result.is_ok(), "Failed to delete builder bid");

        // Validate: builder bid 2 is now the best bid
        let top_bid = cache.get_best_bid(slot, &parent_hash, &proposer_pub_key).await;
        assert!(top_bid.is_ok(), "Failed to get best bid");
        assert_eq!(top_bid.unwrap().unwrap().value(), U256::from(50), "Top bid mismatch");
    }

    #[tokio::test]
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
                &submission.message().proposer_public_key,
            )
            .await
            .unwrap()
            .unwrap_or(U256::ZERO);
        assert!(new_floor_value > floor_value, "Floor value should increase");
    }

    #[tokio::test]
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
                &submission.message().proposer_public_key,
            )
            .await
            .unwrap()
            .unwrap_or(U256::ZERO);
        assert!(new_floor_value == floor_value, "Floor value should not change");
    }

    #[tokio::test]
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
                &submission.message().proposer_public_key,
            )
            .await
            .unwrap()
            .unwrap_or(U256::ZERO);
        assert!(new_floor_value != submission.message().value, "Floor value should not change");
    }

    #[tokio::test]
    async fn test_no_cancellation_bid_above_floor_but_not_top() {
        let (cache, mut submission, floor_value, received_at) = setup_save_and_update_test().await;

        // Save top bid from different builder. Cancellations enabled so won't set new floor.
        submission.message_mut().builder_public_key =
            BlsPublicKey::try_from([53u8; 48].as_ref()).unwrap();
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
        submission.message_mut().builder_public_key = BlsPublicKey::default();
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
        let mut submission = SignedBidSubmission::default();
        submission.message_mut().slot = 1;

        // Save floor value
        submission.message_mut().builder_public_key =
            BlsPublicKey::try_from([12u8; 48].as_ref()).unwrap();
        submission.message_mut().value = floor_value;
        cache
            .save_bid_and_update_top_bid(
                &submission,
                received_at,
                false,
                U256::ZERO,
                &mut state,
                &RelaySigningContext::default(),
            )
            .await
            .unwrap();

        // Reset submission values
        submission.message_mut().builder_public_key = BlsPublicKey::default();
        submission.message_mut().value = U256::from(10);

        (cache, submission, floor_value, received_at)
    }

    #[tokio::test]
    async fn test_seen_or_insert_block_hash() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 42;
        let block_hash = Hash32::try_from([5u8; 32].as_ref()).unwrap();

        // Test: Check if block hash has been seen before (should be false initially)
        let seen_result = cache
            .seen_or_insert_block_hash(
                &block_hash,
                slot,
                &Hash32::default(),
                &BlsPublicKey::default(),
            )
            .await;
        assert!(seen_result.is_ok(), "Failed to check if block hash was seen");
        assert!(!seen_result.unwrap(), "Block hash was incorrectly seen before");

        // Test: Insert the block hash and check again (should be true after insert)
        let seen_result_again = cache
            .seen_or_insert_block_hash(
                &block_hash,
                slot,
                &Hash32::default(),
                &BlsPublicKey::default(),
            )
            .await;
        assert!(seen_result_again.is_ok(), "Failed to check if block hash was seen after insert");
        assert!(seen_result_again.unwrap(), "Block hash was not seen after insert");

        // Test: Add a different new block hash (should be false initially)
        let block_hash_2 = Hash32::try_from([6u8; 32].as_ref()).unwrap();
        let seen_result = cache
            .seen_or_insert_block_hash(
                &block_hash_2,
                slot,
                &Hash32::default(),
                &BlsPublicKey::default(),
            )
            .await;
        assert!(seen_result.is_ok(), "Failed to check if block hash was seen");
        assert!(!seen_result.unwrap(), "Block hash was incorrectly seen before");

        // Test: Insert the original block hash again, ensure it wasn't overwritten (should be true
        // after insert)
        let seen_result_again = cache
            .seen_or_insert_block_hash(
                &block_hash,
                slot,
                &Hash32::default(),
                &BlsPublicKey::default(),
            )
            .await;
        assert!(seen_result_again.is_ok(), "Failed to check if block hash was seen after insert");
        assert!(seen_result_again.unwrap(), "Block hash was not seen after insert");
    }

    #[tokio::test]
    async fn test_can_aquire_lock() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();
        assert!(cache.try_acquire_or_renew_leadership("leader").await)
    }

    #[tokio::test]
    async fn test_others_cant_aquire_lock_if_held() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
        assert!(!cache.try_acquire_or_renew_leadership("others").await);
    }

    #[tokio::test]
    async fn test_can_renew_lock() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
    }

    #[tokio::test]
    async fn test_others_cannot_renew() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
        assert!(cache.try_acquire_or_renew_leadership("leader").await);
        assert!(!cache.try_acquire_or_renew_leadership("others").await);
    }

    #[tokio::test]
    async fn test_pending_blocks() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_infos = vec![BuilderInfoDocument {
            builder_info: BuilderInfo {
                collateral: U256::from(100),
                is_optimistic: true,
                builder_id: None,
            },
            pub_key: BlsPublicKey::default(),
        }];

        cache.update_builder_infos(builder_infos).await.unwrap();

        let slot = 42;
        let block_hash = Hash32::try_from([5u8; 32].as_ref()).unwrap();
        let builder_pub_key = BlsPublicKey::default();
        let time = 1616237123000u64;

        cache.save_pending_block_header(slot, &builder_pub_key, &block_hash, time).await.unwrap();

        cache.save_pending_block_payload(slot, &builder_pub_key, &block_hash, time).await.unwrap();

        let pending_blocks = cache.get_pending_blocks().await.unwrap();

        for i in pending_blocks {
            assert_eq!(i.slot, slot);
            assert_eq!(i.block_hash, block_hash);
            assert_eq!(i.builder_pubkey, builder_pub_key);
            assert_eq!(i.header_receive_ms, Some(time));
            assert_eq!(i.payload_receive_ms, Some(time));
        }
    }

    #[tokio::test]
    async fn test_pending_blocks_multiple() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_infos = vec![BuilderInfoDocument {
            builder_info: BuilderInfo {
                collateral: U256::from(100),
                is_optimistic: true,
                builder_id: None,
            },
            pub_key: BlsPublicKey::default(),
        }];

        cache.update_builder_infos(builder_infos).await.unwrap();

        let mut expected_pending_blocks = Vec::new();

        for i in 0..10 {
            let slot = i as u64;
            let block_hash = Hash32::try_from([i; 32].as_ref()).unwrap();
            let builder_pub_key = BlsPublicKey::default();
            let time = get_current_unix_time_in_nanos() as u64;

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
                builder_pubkey: builder_pub_key,
                header_receive_ms: Some(time),
                payload_receive_ms: Some(time),
            });
        }

        let mut pending_blocks = cache.get_pending_blocks().await.unwrap();
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
    async fn test_pending_blocks_no_header() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_infos = vec![BuilderInfoDocument {
            builder_info: BuilderInfo {
                collateral: U256::from(100),
                is_optimistic: true,
                builder_id: None,
            },
            pub_key: BlsPublicKey::default(),
        }];

        cache.update_builder_infos(builder_infos).await.unwrap();

        let slot = 42;
        let block_hash = Hash32::try_from([5u8; 32].as_ref()).unwrap();
        let builder_pub_key = BlsPublicKey::default();
        let time = 1616237123000u64;

        cache.save_pending_block_payload(slot, &builder_pub_key, &block_hash, time).await.unwrap();

        let pending_blocks = cache.get_pending_blocks().await.unwrap();

        for i in pending_blocks {
            assert_eq!(i.slot, slot);
            assert_eq!(i.block_hash, block_hash);
            assert_eq!(i.builder_pubkey, builder_pub_key);
            assert_eq!(i.header_receive_ms, None);
            assert_eq!(i.payload_receive_ms, Some(time));
        }
    }

    #[tokio::test]
    async fn test_pending_blocks_no_payload() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_infos = vec![BuilderInfoDocument {
            builder_info: BuilderInfo {
                collateral: U256::from(100),
                is_optimistic: true,
                builder_id: None,
            },
            pub_key: BlsPublicKey::default(),
        }];

        cache.update_builder_infos(builder_infos).await.unwrap();

        let slot = 42;
        let block_hash = Hash32::try_from([5u8; 32].as_ref()).unwrap();
        let builder_pub_key = BlsPublicKey::default();
        let time = 1616237123000u64;

        cache.save_pending_block_header(slot, &builder_pub_key, &block_hash, time).await.unwrap();

        let pending_blocks = cache.get_pending_blocks().await.unwrap();

        for i in pending_blocks {
            assert_eq!(i.slot, slot);
            assert_eq!(i.block_hash, block_hash);
            assert_eq!(i.builder_pubkey, builder_pub_key);
            assert_eq!(i.header_receive_ms, Some(time));
            assert_eq!(i.payload_receive_ms, None);
        }
    }

    #[tokio::test]
    async fn test_pending_blocks_dublicate_payload() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_infos = vec![BuilderInfoDocument {
            builder_info: BuilderInfo {
                collateral: U256::from(100),
                is_optimistic: true,
                builder_id: None,
            },
            pub_key: BlsPublicKey::default(),
        }];

        cache.update_builder_infos(builder_infos).await.unwrap();

        let slot = 42;
        let block_hash = Hash32::try_from([5u8; 32].as_ref()).unwrap();
        let builder_pub_key = BlsPublicKey::default();
        let time = 1616237123000u64;

        cache.save_pending_block_header(slot, &builder_pub_key, &block_hash, time).await.unwrap();

        cache.save_pending_block_payload(slot, &builder_pub_key, &block_hash, time).await.unwrap();

        cache
            .save_pending_block_payload(slot, &builder_pub_key, &block_hash, 1716237123000u64)
            .await
            .unwrap();

        let pending_blocks = cache.get_pending_blocks().await.unwrap();

        for i in pending_blocks {
            assert_eq!(i.slot, slot);
            assert_eq!(i.block_hash, block_hash);
            assert_eq!(i.builder_pubkey, builder_pub_key);
            assert_eq!(i.header_receive_ms, Some(time));
            assert_eq!(i.payload_receive_ms, Some(time));
        }
    }

    #[tokio::test]
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
