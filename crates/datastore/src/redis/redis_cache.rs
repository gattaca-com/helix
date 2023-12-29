use std::collections::HashSet;
use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use deadpool_redis::{Config, CreatePoolError, Pool, Runtime};
use ethereum_consensus::{
    clock::get_current_unix_time_in_nanos,
    primitives::{BlsPublicKey, Hash32},
    ssz::prelude::*,
    types::mainnet::ExecutionPayload,
};
use redis::AsyncCommands;
use serde::{de::DeserializeOwned, Serialize};
use helix_database::{error::DatabaseError, DatabaseService};
use helix_common::api::proposer_api::ValidatorRegistrationInfo;
use tracing::error;

use helix_database::types::{
    BidSubmissionDocument, BlockSimErrorDocument, BuilderInfoDocument, DeliveredPayloadDocument,
    DemotionDocument, SubmissionTraceDocument, TooLateGetPayloadDocument,
};
use helix_common::signing::RelaySigningContext;
use helix_common::{
    api::{builder_api::BuilderGetValidatorsResponseEntry, data_api::BidFilters},
    bid_submission::{BidTrace, SignedBidSubmission},
    eth::SignedBuilderBid,
    simulator::BlockSimError,
    BuilderInfo, GetPayloadTrace, SignedValidatorRegistrationEntry, SubmissionTrace,
    ValidatorSummary,
};

use crate::{
    error::AuctioneerError,
    redis::error::RedisCacheError,
    redis::utils::{
        get_builder_latest_bid_time_key, get_builder_latest_bid_value_key, get_cache_bid_trace_key,
        get_cache_get_header_response_key, get_execution_payload_key, get_floor_bid_key,
        get_floor_bid_value_key, get_latest_bid_by_builder_key,
        get_latest_bid_by_builder_key_str_builder_pub_key, get_top_bid_value_key,
    },
    types::{
        keys::{
            BLOCK_SUBMISSION_KEY, BLOCK_SUBMISSION_TRACE_KEY, BUILDER_INFO_KEY,
            DELIVERED_PAYLOAD_KEY, DEMOTIONS_KEY, GET_PAYLOAD_TOO_LATE_KEY, KNOWN_VALIDATORS_KEY,
            LAST_HASH_DELIVERED_KEY, LAST_SLOT_DELIVERED_KEY, PROPOSER_DUTIES_KEY,
            SIMULATION_ERROR_KEY, VALIDATOR_REGISTRATIONS_KEY,
        },
        SaveBidAndUpdateTopBidResponse,
    },
    Auctioneer,
};

const BID_CACHE_EXPIRY_S: usize = 45;

#[derive(Clone)]
pub struct RedisCache {
    pool: Pool,
}

impl RedisCache {
    pub async fn new(
        conn_str: &str,
        builder_infos: Vec<BuilderInfoDocument>,
    ) -> Result<Self, CreatePoolError> {
        let cfg = Config::from_url(conn_str);
        let pool = cfg.create_pool(Some(Runtime::Tokio1))?;
        let cache = Self { pool };

        // Load in builder info
        if let Err(err) = cache.update_builder_infos(builder_infos).await {
            error!(err=%err, "Failed to initialise builder info")
        }

        Ok(cache)
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

    async fn rpush(&self, key: &str, value: &impl Serialize) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        let str_val = serde_json::to_string(value)?;
        Ok(conn.rpush(key, str_val).await?)
    }

    async fn hdel(&self, key: &str, field: &str) -> Result<(), RedisCacheError> {
        let mut conn = self.pool.get().await?;
        Ok(conn.hdel(key, field).await?)
    }

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
    ///
    /// This function performs the following steps:
    /// 1. Check if there are any builder bids; if not, exit early.
    /// 2. Determine the current top bid among the builder bids.
    /// 3. Compare the top bid with a floor value. Use the greater of the two.
    /// 4. Update the get_header response to reflect the new top bid.
    /// 5. Update the global top bid value.
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
                    &proposer_pub_key,
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
        new_floor_value: &U256,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<(), RedisCacheError> {
        let key_bid_source =
            get_latest_bid_by_builder_key(slot, parent_hash, &proposer_pub_key, &builder_pub_key);
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
                return Ok(());
            }
        }

        self.set(LAST_SLOT_DELIVERED_KEY, &slot, None).await?;
        self.set(LAST_HASH_DELIVERED_KEY, &format!("{hash:?}"), None).await?;
        Ok(())
    }

    async fn get_best_bid(
        &self,
        slot: u64,
        parent_hash: &Hash32,
        proposer_pub_key: &BlsPublicKey,
    ) -> Result<Option<SignedBuilderBid>, AuctioneerError> {
        let key = get_cache_get_header_response_key(slot, parent_hash, proposer_pub_key);
        Ok(self.get(&key).await?)
    }

    async fn save_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &Hash32,
        execution_payload: &ExecutionPayload,
    ) -> Result<(), AuctioneerError> {
        let key = get_execution_payload_key(slot, proposer_pub_key, block_hash);
        Ok(self.set(&key, &execution_payload, Some(BID_CACHE_EXPIRY_S)).await?)
    }

    async fn get_execution_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        block_hash: &Hash32,
    ) -> Result<Option<ExecutionPayload>, AuctioneerError> {
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
        builder_bid: SignedBuilderBid,
    ) -> Result<(), AuctioneerError> {
        let mut conn = self.pool.get().await.map_err(RedisCacheError::from)?;
        let mut pipe = redis::pipe();

        let serialised_bid = serde_json::to_string(&builder_bid).map_err(RedisCacheError::from)?;
        let serialised_value =
            serde_json::to_string(&builder_bid.value()).map_err(RedisCacheError::from)?;

        let key_latest_bid =
            get_latest_bid_by_builder_key(slot, parent_hash, &proposer_pub_key, &builder_pub_key);
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
        submission: Arc<SignedBidSubmission>,
        received_at: u128,
        cancellations_enabled: bool,
        floor_value: U256,
        state: &mut SaveBidAndUpdateTopBidResponse,
        signing_context: &RelaySigningContext,
    ) -> Result<(), AuctioneerError> {
        let slot = submission.message.slot;
        let parent_hash = submission.execution_payload.parent_hash();
        let proposer_pub_key = &submission.message.proposer_public_key;
        let builder_pub_key = &submission.message.builder_public_key;

        // Load the latest bids from all builders for the current slot, parent hash, and proposer
        let mut builder_bids =
            self.get_new_builder_bids(slot, parent_hash, proposer_pub_key).await?;

        // Get the current top bid. It will be the max of all builder bids and the current floor.
        state.top_bid_value =
            builder_bids.values().max().cloned().unwrap_or(U256::ZERO).max(floor_value.clone());

        state.prev_top_bid_value = state.top_bid_value.clone();
        let is_bid_above_floor = submission.message.value > floor_value;

        // Exit early if cancellations aren't enabled and the bid is below the floor.
        if !cancellations_enabled && !is_bid_above_floor {
            return Ok(());
        }
        state.set_latency_get_prev_top_bid();

        // Save the execution payload
        self.save_execution_payload(
            slot,
            proposer_pub_key,
            submission.execution_payload.block_hash(),
            &submission.execution_payload,
        )
        .await?;
        state.set_latency_save_payload();

        // Save the latest bid for this builder
        let mut cloned_submission = (*submission).clone(); // TODO: really need to fix this clone
        let builder_bid = SignedBuilderBid::from_submission(
            &mut cloned_submission,
            signing_context.public_key.clone(),
            &signing_context.signing_key,
            &signing_context.context,
        )?;
        self.save_builder_bid(
            slot,
            parent_hash,
            proposer_pub_key,
            builder_pub_key,
            received_at,
            builder_bid,
        )
        .await?;
        state.was_bid_saved = true;
        builder_bids.insert(format!("{builder_pub_key:?}"), submission.message.value.clone());
        state.set_latency_save_bid();

        // Save the bid trace
        self.save_bid_trace(&submission.message).await?;
        state.set_latency_save_trace();

        // Abort if the top bid hasn't changed
        // TODO: the floor may have raised but we will exit early here.
        state.top_bid_value = builder_bids.values().max().cloned().unwrap_or(U256::ZERO);
        if state.top_bid_value == state.prev_top_bid_value {
            return Ok(());
        }

        // Update the top bid
        self.update_top_bid(state, &builder_bids, slot, parent_hash, proposer_pub_key, floor_value)
            .await?;
        state.is_new_top_bid = submission.message.value == state.top_bid_value;
        state.set_latency_update_top_bid();

        // Handle floor value updates only if needed.
        // Only non-cancellable bids above the floor should set a new floor.
        if cancellations_enabled || !is_bid_above_floor {
            return Ok(());
        }
        self.set_new_floor(
            &submission.message.value,
            slot,
            parent_hash,
            proposer_pub_key,
            builder_pub_key,
        )
        .await?;
        state.set_latency_update_floor();

        Ok(())
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
            return Ok(());
        }
        builder_info.is_optimistic = false;
        Ok(self.hset(BUILDER_INFO_KEY, &format!("{builder_pub_key:?}"), &builder_info).await?)
    }

    async fn update_builder_infos(
        &self,
        builder_infos: Vec<BuilderInfoDocument>,
    ) -> Result<(), AuctioneerError> {
        if builder_infos.is_empty() {
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
}

fn get_top_bid(bid_values: &HashMap<String, U256>) -> Option<(String, U256)> {
    bid_values
        .iter()
        .max_by_key(|&(_, value)| value)
        .map(|(key, value)| (key.clone(), value.clone()))
}

#[async_trait]
impl DatabaseService for RedisCache {
    async fn save_validator_registration(
        &self,
        entry: ValidatorRegistrationInfo,
    ) -> Result<(), DatabaseError> {
        let public_key = format!("{:?}", entry.registration.message.public_key); //
        let entry = SignedValidatorRegistrationEntry::new(entry);
        self.hset(VALIDATOR_REGISTRATIONS_KEY, &public_key, &entry)
            .await
            .map_err(|err| DatabaseError::RedisError(err.to_string()))?;
        Ok(())
    }

    async fn save_validator_registrations(
        &self,
        entries: Vec<ValidatorRegistrationInfo>,
    ) -> Result<(), DatabaseError> {
        todo!()
    }

    async fn get_validator_registration(
        &self,
        pub_key: BlsPublicKey,
    ) -> Result<SignedValidatorRegistrationEntry, DatabaseError> {
        self.hget(VALIDATOR_REGISTRATIONS_KEY, &format!("{pub_key:?}"))
            .await
            .map_err(|err| DatabaseError::RedisError(err.to_string()))?
            .ok_or(DatabaseError::ValidatorRegistrationNotFound)
    }

    async fn get_validator_registrations_for_pub_keys(
        &self,
        pub_keys: Vec<BlsPublicKey>,
    ) -> Result<Vec<SignedValidatorRegistrationEntry>, DatabaseError> {
        let mut results = Vec::with_capacity(pub_keys.len());
        for pub_key in pub_keys.iter() {
            match self.get_validator_registration(pub_key.clone()).await {
                Ok(registration) => results.push(registration),
                Err(DatabaseError::ValidatorRegistrationNotFound) => {}
                Err(err) => {
                    return Err(err);
                }
            }
        }
        Ok(results)
    }

    async fn get_validator_registration_timestamp(
        &self,
        pub_key: BlsPublicKey,
    ) -> Result<u64, DatabaseError> {
        self.get_validator_registration(pub_key).await.map(|registration| registration.inserted_at)
    }

    async fn set_proposer_duties(
        &self,
        proposer_duties: Vec<BuilderGetValidatorsResponseEntry>,
    ) -> Result<(), DatabaseError> {
        self.set(PROPOSER_DUTIES_KEY, &proposer_duties, None)
            .await
            .map_err(|err| DatabaseError::RedisError(err.to_string()))?;
        Ok(())
    }

    async fn get_proposer_duties(
        &self,
    ) -> Result<Vec<BuilderGetValidatorsResponseEntry>, DatabaseError> {
        self.get(PROPOSER_DUTIES_KEY)
            .await
            .map_err(|err| DatabaseError::RedisError(err.to_string()))?
            .ok_or(DatabaseError::ProposerDutiesNotFound)
    }

    async fn set_known_validators(
        &self,
        known_validators: Vec<ValidatorSummary>,
    ) -> Result<(), DatabaseError> {
        self.clear_key(KNOWN_VALIDATORS_KEY)
            .await
            .map_err(|err| DatabaseError::RedisError(err.to_string()))?;
        for validator in known_validators.iter() {
            let pub_key = &validator.validator.public_key;
            if let Err(err) =
                self.hset(KNOWN_VALIDATORS_KEY, &format!("{pub_key:?}"), &validator.index).await
            {
                error!(err=%err, "error setting known validator");
            }
        }
        Ok(())
    }

    // async fn get_validator_index(
    //     &self,
    //     public_key: &BlsPublicKey,
    // ) -> Result<ValidatorIndex, DatabaseError> {
    //     self.hget(KNOWN_VALIDATORS_KEY, &format!("{public_key:?}"))
    //         .await?
    //         .ok_or(DatabaseError::ValidatorNotFound { public_key: public_key.clone() })
    // }

    async fn check_known_validators(
        &self,
        public_keys: Vec<BlsPublicKey>,
    ) -> Result<HashSet<BlsPublicKey>, DatabaseError> {
        //     self.hget(KNOWN_VALIDATORS_KEY, &format!("{public_key:?}"))
        //         .await?
        //         .ok_or(DatabaseError::ValidatorNotFound { public_key: public_key.clone() })
        todo!()
    }

    async fn save_too_late_get_payload(
        &self,
        slot: u64,
        proposer_pub_key: &BlsPublicKey,
        payload_hash: &Hash32,
        message_received: u64,
        payload_fetched: u64,
    ) -> Result<(), DatabaseError> {
        let too_late_get_payload = TooLateGetPayloadDocument {
            slot,
            proposer_pub_key: proposer_pub_key.clone(),
            payload_hash: payload_hash.clone(),
            message_received,
            payload_fetched,
        };

        self.rpush(GET_PAYLOAD_TOO_LATE_KEY, &too_late_get_payload)
            .await
            .map_err(|err| DatabaseError::RedisError(err.to_string()))?;
        Ok(())
    }

    async fn save_delivered_payload(
        &self,
        bid_trace: &BidTrace,
        payload: Arc<ExecutionPayload>,
        latency_trace: &GetPayloadTrace,
    ) -> Result<(), DatabaseError> {
        let delivered_payload = DeliveredPayloadDocument {
            bid_trace: bid_trace.clone(),
            payload,
            latency_trace: latency_trace.clone(),
        };

        self.rpush(DELIVERED_PAYLOAD_KEY, &delivered_payload)
            .await
            .map_err(|err| DatabaseError::RedisError(err.to_string()))?;
        Ok(())
    }

    async fn store_block_submission(
        &self,
        submission: Arc<SignedBidSubmission>,
    ) -> Result<(), DatabaseError> {
        let document = BidSubmissionDocument::from_signed_submission(&submission);
        self.rpush(BLOCK_SUBMISSION_KEY, &document)
            .await
            .map_err(|err| DatabaseError::RedisError(err.to_string()))?;
        Ok(())
    }

    async fn save_block_submission_trace(
        &self,
        block_hash: Hash32,
        trace: SubmissionTrace,
    ) -> Result<(), DatabaseError> {
        let submission_trace_doc = SubmissionTraceDocument { block_hash, trace };
        self.rpush(BLOCK_SUBMISSION_TRACE_KEY, &submission_trace_doc)
            .await
            .map_err(|err| DatabaseError::RedisError(err.to_string()))?;
        Ok(())
    }

    async fn store_builder_info(
        &self,
        builder_pub_key: &BlsPublicKey,
        builder_info: BuilderInfo,
    ) -> Result<(), DatabaseError> {
        self.hset(BUILDER_INFO_KEY, &format!("{builder_pub_key:?}"), &builder_info)
            .await
            .map_err(|err| DatabaseError::RedisError(err.to_string()))?;
        Ok(())
    }

    async fn db_get_builder_info(
        &self,
        builder_pub_key: &BlsPublicKey,
    ) -> Result<BuilderInfo, DatabaseError> {
        self.hget(BUILDER_INFO_KEY, &format!("{builder_pub_key:?}"))
            .await
            .map_err(|err| DatabaseError::RedisError(err.to_string()))?
            .ok_or(DatabaseError::BuilderInfoNotFound { public_key: builder_pub_key.clone() })
    }

    async fn get_all_builder_infos(&self) -> Result<Vec<BuilderInfoDocument>, DatabaseError> {
        let builder_infos: HashMap<String, BuilderInfo> = self
            .hgetall(BUILDER_INFO_KEY)
            .await
            .map_err(|err| DatabaseError::RedisError(err.to_string()))?
            .ok_or(DatabaseError::AllBuilderInfoNotFound)?;

        let mut res = Vec::with_capacity(builder_infos.len());
        for (pub_key, builder_info) in builder_infos {
            let pub_key = BlsPublicKey::try_from(hex::decode(pub_key)?.as_slice())?;
            res.push(BuilderInfoDocument { pub_key, builder_info })
        }

        Ok(res)
    }

    async fn db_demote_builder(&self, builder_pub_key: &BlsPublicKey) -> Result<(), DatabaseError> {
        let mut builder_info = self.db_get_builder_info(builder_pub_key).await?;
        if !builder_info.is_optimistic {
            return Ok(());
        }

        builder_info.is_optimistic = false;
        self.store_builder_info(builder_pub_key, builder_info).await?;

        // Write to demotions table
        let demotion = DemotionDocument {
            pub_key: builder_pub_key.clone(),
            demotion_time: get_current_unix_time_in_nanos() as u64,
        };
        self.rpush(DEMOTIONS_KEY, &demotion)
            .await
            .map_err(|err| DatabaseError::RedisError(err.to_string()))?;

        Ok(())
    }

    async fn save_simulation_result(
        &self,
        block_hash: ByteVector<32>,
        block_sim_result: Result<(), BlockSimError>,
    ) -> Result<(), DatabaseError> {
        if let Err(err) = block_sim_result {
            let block_sim_document = BlockSimErrorDocument { block_hash, sim_error: err };
            self.rpush(SIMULATION_ERROR_KEY, &block_sim_document)
                .await
                .map_err(|err| DatabaseError::RedisError(err.to_string()))?;
        }
        Ok(())
    }

    /// Fetches bids from Redis and filters them based on the provided filters.
    /// This is very inefficient as it first fetches every submission. Only use this db for testing.
    async fn get_bids(
        &self,
        filters: &BidFilters,
    ) -> Result<Vec<BidSubmissionDocument>, DatabaseError> {
        // Step 1: Fetch all bids from Redis.
        let mut bids: Vec<BidSubmissionDocument> =
            self.lrange(BLOCK_SUBMISSION_KEY, 0, -1)
                .await
                .map_err(|err| DatabaseError::RedisError(err.to_string()))?;

        // Step2: Filter the bids.
        bids.retain(|submission| !should_filter_submission(submission, filters));

        // Step 3: Apply limiting.
        if let Some(limit) = filters.limit {
            bids.truncate(limit as usize);
        }

        Ok(bids)
    }

    /// Fetches bids from Redis and filters them based on the provided filters.
    /// This is very inefficient as it first fetches every submission. Only use this db for testing.
    async fn get_delivered_payloads(
        &self,
        filters: &BidFilters,
    ) -> Result<Vec<DeliveredPayloadDocument>, DatabaseError> {
        // Step 1: Fetch all delivered_payloads from Redis.
        let mut delivered_payloads: Vec<DeliveredPayloadDocument> = self
            .lrange(DELIVERED_PAYLOAD_KEY, 0, -1)
            .await
            .map_err(|err| DatabaseError::RedisError(err.to_string()))?;

        // Step2: Filter the bids.
        delivered_payloads.retain(|payload| !should_filter_payload(payload, filters));

        // Step 3: Apply sorting and limiting.
        if let Some(order_by) = filters.order_by {
            delivered_payloads.sort_by_key(|a| a.value().clone());
            if order_by < 0 {
                delivered_payloads.reverse();
            }
        }

        if let Some(limit) = filters.limit {
            delivered_payloads.truncate(limit as usize);
        }

        Ok(delivered_payloads)
    }
}

fn should_filter_submission(submission: &BidSubmissionDocument, filters: &BidFilters) -> bool {
    if let Some(slot) = filters.slot {
        if submission.bid_trace.slot != slot {
            return true;
        }
    }

    if let Some(block_hash) = &filters.block_hash {
        if &submission.bid_trace.block_hash != block_hash {
            return true;
        }
    }

    if let Some(block_number) = filters.block_number {
        if submission.block_number != block_number {
            return true;
        }
    }

    if let Some(builder_pubkey) = &filters.builder_pubkey {
        if &submission.bid_trace.builder_public_key != builder_pubkey {
            return true;
        }
    }

    false
}

fn should_filter_payload(payload: &DeliveredPayloadDocument, filters: &BidFilters) -> bool {
    if let Some(slot) = filters.slot {
        if payload.bid_trace.slot != slot {
            return true;
        }
    }

    if let Some(block_hash) = &filters.block_hash {
        if payload.payload.block_hash() != block_hash {
            return true;
        }
    }

    if let Some(block_number) = filters.block_number {
        if payload.payload.block_number() != block_number {
            return true;
        }
    }

    if let Some(builder_pubkey) = &filters.builder_pubkey {
        if &payload.bid_trace.builder_public_key != builder_pubkey {
            return true;
        }
    }

    if let Some(proposer_pubkey) = &filters.proposer_pubkey {
        if &payload.bid_trace.proposer_public_key != proposer_pubkey {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {

    use super::*;
    use ethereum_consensus::phase0::Validator;
    use serde::{Deserialize, Serialize};
    use helix_common::capella::{self, ExecutionPayloadHeader};

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
        let slot = 5 as u64;
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
                prev_best_bid,
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
                floor_value.clone(),
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
        let key_top_bid = get_top_bid_value_key(slot, &parent_hash, &proposer_pub_key);
        let mut conn = cache.pool.get().await.unwrap();

        let cache_value =
            cache.get_top_bid_value(slot, &parent_hash, &proposer_pub_key).await.unwrap();
        assert_eq!(cache_value, Some(U256::from(60)), "Cache value mismatch");

        // Test with floor_value greater than top_bid_value
        let higher_floor_value = U256::from(70);
        capella_builder_bid.value = higher_floor_value.clone();
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
        let execution_payload = ExecutionPayload::Capella(capella_payload);

        // Save the execution payload
        let save_result = cache
            .save_execution_payload(slot, &proposer_pub_key, &block_hash, &execution_payload)
            .await;
        assert!(save_result.is_ok(), "Failed to save the execution payload");

        // Test: Get the execution payload
        let get_result: Result<Option<ExecutionPayload>, _> =
            cache.get_execution_payload(slot, &proposer_pub_key, &block_hash).await;
        assert!(get_result.is_ok(), "Failed to get the execution payload");
        assert!(get_result.as_ref().unwrap().is_some(), "Execution payload is None");

        let fetched_execution_payload = get_result.unwrap().unwrap();
        assert_eq!(fetched_execution_payload.gas_limit(), 999, "Execution payload mismatch");
    }

    #[tokio::test]
    async fn test_get_and_save_bid_trace() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let mut bid_trace = BidTrace::default();
        bid_trace.gas_used = 999;
        bid_trace.slot = 42;

        // Save the bid trace
        let save_result = cache.save_bid_trace(&bid_trace).await;
        assert!(save_result.is_ok(), "Failed to save the bid trace");

        // Test: Get the bid trace
        let get_result: Result<Option<BidTrace>, _> = cache
            .get_bid_trace(bid_trace.slot, &bid_trace.proposer_public_key, &bid_trace.block_hash)
            .await;
        assert!(get_result.is_ok(), "Failed to get the bid trace");
        assert!(get_result.as_ref().unwrap().is_some(), "Bid trace is None");
        assert_eq!(get_result.unwrap().unwrap().gas_used, 999, "Bid trace mismatch");
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
                builder_bid.clone(),
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

        let builder_info = BuilderInfo { collateral: U256::from(12), is_optimistic: true };

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
    async fn test_demote_non_optimistic_builder() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key = BlsPublicKey::try_from([23u8; 48].as_ref()).unwrap();
        let builder_info = BuilderInfo { collateral: U256::from(12), is_optimistic: false };

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
        let builder_info = BuilderInfo { collateral: U256::from(12), is_optimistic: true };

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
                builder_bid_1,
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
                builder_bid_2,
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
                submission.clone().into(),
                received_at,
                false,
                floor_value,
                &mut state,
                &RelaySigningContext::default(),
            )
            .await;
        assert!(result.is_ok(), "Save failed");
        assert_eq!(state.was_bid_saved, false);
        assert_eq!(state.is_new_top_bid, false);
    }

    #[tokio::test]
    async fn test_no_cancellation_bid_above_floor() {
        let (cache, mut submission, floor_value, received_at) = setup_save_and_update_test().await;
        let mut state = SaveBidAndUpdateTopBidResponse::default();

        submission.message.value = floor_value + U256::from(1);
        let result = cache
            .save_bid_and_update_top_bid(
                submission.clone().into(),
                received_at,
                false,
                floor_value,
                &mut state,
                &RelaySigningContext::default(),
            )
            .await;
        assert!(result.is_ok(), "Save failed");
        assert_eq!(state.was_bid_saved, true, "Bid should be saved");
        assert_eq!(state.is_new_top_bid, true, "Bid should be new top bid");

        // Validate bid is new floor
        let new_floor_value = cache
            .get_floor_bid_value(
                submission.message.slot,
                &submission.message.parent_hash,
                &submission.message.proposer_public_key,
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

        submission.message.value = floor_value - U256::from(1);
        let result = cache
            .save_bid_and_update_top_bid(
                submission.clone().into(),
                received_at,
                true,
                floor_value,
                &mut state,
                &RelaySigningContext::default(),
            )
            .await;
        assert!(result.is_ok(), "Save failed");
        assert_eq!(state.was_bid_saved, true, "Bid should be saved");
        assert_eq!(state.is_new_top_bid, false, "Bid should not be new top bid");

        // Validate floor is the same
        let new_floor_value = cache
            .get_floor_bid_value(
                submission.message.slot,
                &submission.message.parent_hash,
                &submission.message.proposer_public_key,
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

        submission.message.value = floor_value + U256::from(1);
        let result = cache
            .save_bid_and_update_top_bid(
                submission.clone().into(),
                received_at,
                true,
                floor_value,
                &mut state,
                &RelaySigningContext::default(),
            )
            .await;
        assert!(result.is_ok(), "Failed to save bid");
        assert_eq!(state.was_bid_saved, true, "Bid should be saved");
        assert_eq!(state.is_new_top_bid, true, "Bid should be new top bid");

        // Validate bid is not new floor as this is a cancellable bid
        let new_floor_value = cache
            .get_floor_bid_value(
                submission.message.slot,
                &submission.message.parent_hash,
                &submission.message.proposer_public_key,
            )
            .await
            .unwrap()
            .unwrap_or(U256::ZERO);
        assert!(new_floor_value != submission.message.value, "Floor value should not change");
    }

    #[tokio::test]
    async fn test_no_cancellation_bid_above_floor_but_not_top() {
        let (cache, mut submission, floor_value, received_at) = setup_save_and_update_test().await;

        // Save top bid from different builder. Cancellations enabled so won't set new floor.
        submission.message.builder_public_key =
            BlsPublicKey::try_from([53u8; 48].as_ref()).unwrap();
        submission.message.value = floor_value + U256::from(2);
        let mut state = SaveBidAndUpdateTopBidResponse::default();
        let result = cache
            .save_bid_and_update_top_bid(
                submission.clone().into(),
                received_at,
                true,
                floor_value.clone(),
                &mut state,
                &RelaySigningContext::default(),
            )
            .await;
        assert!(result.is_ok(), "Failed to save top bid");

        // Save bid below top bid but above floor.
        submission.message.value = floor_value + U256::from(1);
        submission.message.builder_public_key = BlsPublicKey::default();
        let mut state = SaveBidAndUpdateTopBidResponse::default();

        let result = cache
            .save_bid_and_update_top_bid(
                submission.clone().into(),
                received_at,
                false,
                floor_value,
                &mut state,
                &RelaySigningContext::default(),
            )
            .await;
        assert!(result.is_ok(), "Failed to save bid");
        assert_eq!(state.was_bid_saved, true, "Bid should be saved");
        assert_eq!(state.is_new_top_bid, false, "Bid should not be the new top bid");
    }

    async fn setup_save_and_update_test() -> (RedisCache, SignedBidSubmission, U256, u128) {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let floor_value = U256::from(50);
        let received_at = 1000;

        let mut state = SaveBidAndUpdateTopBidResponse::default();
        let mut submission = SignedBidSubmission::default();
        submission.message.slot = 1;

        // Save floor value
        submission.message.builder_public_key =
            BlsPublicKey::try_from([12u8; 48].as_ref()).unwrap();
        submission.message.value = floor_value.clone();
        cache
            .save_bid_and_update_top_bid(
                submission.clone().into(),
                received_at,
                false,
                U256::ZERO,
                &mut state,
                &RelaySigningContext::default(),
            )
            .await
            .unwrap();

        // Reset submission values
        submission.message.builder_public_key = BlsPublicKey::default();
        submission.message.value = U256::from(10);

        (cache, submission, floor_value, received_at)
    }

    /// #######################################################################
    /// ######################## DatabaseService tests ########################
    /// #######################################################################

    #[tokio::test]
    async fn test_validator_registrations() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let pub_key_1 = BlsPublicKey::try_from([1u8; 48].as_ref()).unwrap();

        let mut entry1 = ValidatorRegistrationInfo::default();
        entry1.registration.message.public_key = pub_key_1.clone();

        let pub_key_2 = BlsPublicKey::try_from([2u8; 48].as_ref()).unwrap();
        let mut entry2 = ValidatorRegistrationInfo::default();
        entry2.registration.message.public_key = pub_key_2.clone();

        // Test: Save the first validator registration
        let result = cache.save_validator_registration(entry1.clone()).await;
        assert!(result.is_ok(), "Saving first validator registration failed");

        // Test: The registration can be fetched
        let get_result = cache.get_validator_registration(pub_key_1.clone()).await;
        assert!(get_result.is_ok(), "Fetching first validator registration failed");
        assert_eq!(
            get_result.unwrap().public_key(),
            &pub_key_1,
            "Public key mismatch for the first registration"
        );

        // Test: We can save and fetch multiple registrations
        let result = cache.save_validator_registration(entry2.clone()).await;
        assert!(result.is_ok(), "Saving second validator registration failed");

        let pub_keys = vec![pub_key_1.clone(), pub_key_2.clone()];
        let get_result = cache.get_validator_registrations_for_pub_keys(pub_keys).await;
        assert!(get_result.is_ok(), "Fetching multiple validator registrations failed");
        assert!(get_result.unwrap().len() == 2, "Incorrect number of entries fetched");

        // Test: The registration timestamp can be fetched
        let result = cache.get_validator_registration_timestamp(pub_key_1.clone()).await;
        assert!(result.is_ok(), "Fetching timestamp for the first registration failed");

        // Test: Unknown pubkeys return a NotFoundError
        let unknown_pub_key = BlsPublicKey::try_from([5u8; 48].as_ref()).unwrap();
        let result = cache.get_validator_registration(unknown_pub_key.clone()).await;
        assert!(
            matches!(result, Err(DatabaseError::ValidatorRegistrationNotFound)),
            "Incorrect error for unknown pubkey"
        );
    }

    #[tokio::test]
    async fn test_get_and_set_proposer_duties() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let duties = vec![BuilderGetValidatorsResponseEntry {
            slot: 42,
            validator_index: 0,
            entry: ValidatorRegistrationInfo::default(),
        }];

        // Test: Setting proposer duties works
        let set_result = cache.set_proposer_duties(duties.clone()).await;
        assert!(set_result.is_ok(), "Setting proposer duties failed");

        // Test: Getting proposer duties works
        let get_result = cache.get_proposer_duties().await;
        let retrieved_duties = get_result.unwrap();

        assert_eq!(
            retrieved_duties.len(),
            duties.len(),
            "Incorrect number of proposer duties returned"
        );
        for (retrieved, original) in retrieved_duties.iter().zip(duties.iter()) {
            assert_eq!(retrieved.slot, original.slot, "Slot mismatch");
            assert_eq!(
                retrieved.validator_index, original.validator_index,
                "Validator index mismatch"
            );
        }
    }

    #[tokio::test]
    async fn test_get_and_set_known_validators() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let validator = ValidatorSummary {
            index: 324,
            balance: 0,
            status: helix_common::ValidatorStatus::ActiveOngoing,
            validator: Validator::default(),
        };

        // Test: Setting known validators works
        let set_result = cache.set_known_validators(vec![validator.clone()]).await;
        assert!(set_result.is_ok(), "Setting known validators failed");

        // Test: Gettting validator index from known validators works
        let get_result =
            cache.check_known_validators(vec![validator.validator.public_key.clone()]).await;
        assert!(get_result.is_ok(), "Getting validator index from known validators failed");

        // Test: Getting unknown validator index returns not found error
        let unknown_pub_key = BlsPublicKey::try_from([5u8; 48].as_ref()).unwrap();
        let get_result = cache.check_known_validators(vec![unknown_pub_key.clone()]).await;
        assert!(
            matches!(get_result, Err(DatabaseError::ValidatorNotFound { .. })),
            "Incorrect error for unknown validator pubkey"
        );

        // Test: Setting known validators overwrites current validators
        let mut new_validator = validator.clone();
        new_validator.validator.public_key = unknown_pub_key;
        let set_result = cache.set_known_validators(vec![new_validator.clone()]).await;
        assert!(set_result.is_ok(), "Setting new known validators failed");

        let get_result =
            cache.check_known_validators(vec![validator.validator.public_key.clone()]).await;
        assert!(
            matches!(get_result, Err(DatabaseError::ValidatorNotFound { .. })),
            "Incorrect error for fetching old validator pubkey"
        );
    }

    #[tokio::test]
    async fn test_save_too_late_get_payload() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let slot = 100;
        let proposer_pub_key = BlsPublicKey::try_from([3u8; 48].as_ref()).unwrap();
        let payload_hash = Hash32::try_from([4u8; 32].as_ref()).unwrap();
        let message_received = 200;
        let payload_fetched = 300;

        // Test: Save the too-late payload data
        let result = cache
            .save_too_late_get_payload(
                slot,
                &proposer_pub_key,
                &payload_hash,
                message_received,
                payload_fetched,
            )
            .await;
        assert!(result.is_ok(), "Saving too-late payload data failed");

        // Validate: Manually fetch and check the stored data
        let stored_data: Result<Vec<TooLateGetPayloadDocument>, _> =
            cache.lrange(GET_PAYLOAD_TOO_LATE_KEY, 0, -1).await;
        assert!(stored_data.is_ok(), "Fetching stored too-late payload data failed");
        assert_eq!(stored_data.unwrap().len(), 1, "Incorrect number of too-late payload entries");

        // Test: Store another
        let result = cache
            .save_too_late_get_payload(
                slot,
                &proposer_pub_key,
                &payload_hash,
                message_received,
                payload_fetched,
            )
            .await;
        assert!(result.is_ok(), "Saving too-late payload data failed");

        // Validate: Check there are now two too late payloads
        let stored_data: Result<Vec<TooLateGetPayloadDocument>, _> =
            cache.lrange(GET_PAYLOAD_TOO_LATE_KEY, 0, -1).await;
        assert!(stored_data.is_ok(), "Fetching stored too-late payload data failed");
        assert_eq!(stored_data.unwrap().len(), 2, "Incorrect number of too-late payload entries");
    }

    #[tokio::test]
    async fn test_save_delivered_payload() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let bid_trace = BidTrace::default();
        let payload = Arc::new(ExecutionPayload::Capella(capella::ExecutionPayload::default()));
        let latency_trace = GetPayloadTrace::default();

        // Test: Save the delivered payload data
        let result =
            cache.save_delivered_payload(&bid_trace, payload.clone(), &latency_trace).await;
        assert!(result.is_ok(), "Saving delivered payload data failed.");

        // Validate: Manually fetch and check the stored data
        let stored_data: Result<Vec<DeliveredPayloadDocument>, _> =
            cache.lrange(DELIVERED_PAYLOAD_KEY, 0, -1).await;
        assert!(stored_data.is_ok(), "Fetching stored delivered payload data failed");
        assert_eq!(stored_data.unwrap().len(), 1, "Incorrect number of delivered payload entries");
    }

    #[tokio::test]
    async fn test_store_block_submission() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let submission = Arc::new(SignedBidSubmission::default());

        // Test: Save the block submission data
        let result = cache.store_block_submission(submission.clone()).await;
        assert!(result.is_ok(), "Saving block submission data failed");

        // Validate: Manually fetch and check the stored data
        let stored_data: Result<Vec<SignedBidSubmission>, _> =
            cache.lrange(BLOCK_SUBMISSION_KEY, 0, -1).await;
        assert!(stored_data.is_ok(), "Fetching stored block submission data failed");
        assert_eq!(stored_data.unwrap().len(), 1, "Incorrect number of block submission entries");
    }

    #[tokio::test]
    async fn test_save_block_submission_trace() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let block_hash = Hash32::try_from([4u8; 32].as_ref()).unwrap();
        let trace = SubmissionTrace::default();

        // Test: Save the block submission trace
        let result = cache.save_block_submission_trace(block_hash, trace.clone()).await;
        assert!(result.is_ok(), "Saving block submission trace failed");

        // Validate: Manually fetch and check the stored data
        let stored_data: Result<Vec<SubmissionTraceDocument>, _> =
            cache.lrange(BLOCK_SUBMISSION_TRACE_KEY, 0, -1).await;
        assert!(stored_data.is_ok(), "Fetching stored block submission trace data failed");
        assert_eq!(
            stored_data.unwrap().len(),
            1,
            "Incorrect number of block submission trace entries"
        );
    }

    #[tokio::test]
    async fn test_store_and_get_builder_info() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key = BlsPublicKey::try_from([2u8; 48].as_ref()).unwrap();
        let builder_info = BuilderInfo::default();

        // Test: Store the builder info
        let store_result = cache.store_builder_info(&builder_pub_key, builder_info.clone()).await;
        assert!(store_result.is_ok(), "Storing builder info failed");

        // Test: Retrieve the stored builder info
        let get_result = cache.db_get_builder_info(&builder_pub_key).await;
        assert!(get_result.is_ok(), "Fetching stored builder info failed");
        assert_eq!(
            get_result.as_ref().unwrap().collateral,
            builder_info.collateral,
            "Builder info collateral mismatch"
        );
        assert_eq!(
            get_result.unwrap().is_optimistic,
            builder_info.is_optimistic,
            "Builder info is_optimistic mismatch"
        );
    }

    #[tokio::test]
    async fn test_get_all_builder_infos() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key = BlsPublicKey::try_from([3u8; 48].as_ref()).unwrap();
        let builder_info = BuilderInfo::default();

        // Store some builder info to fetch later
        cache.store_builder_info(&builder_pub_key, builder_info.clone()).await.unwrap();

        // Test: Retrieve all stored builder infos
        let get_result = cache.get_all_builder_infos().await;
        assert!(get_result.is_ok(), "Fetching all builder infos failed");
        assert_eq!(get_result.unwrap().len(), 1, "Incorrect number of builder info entries");

        // Store another builder to fetch later
        let builder_pub_key_2 = BlsPublicKey::try_from([5u8; 48].as_ref()).unwrap();
        cache.store_builder_info(&builder_pub_key_2, builder_info.clone()).await.unwrap();

        // Test: Retrieve all stored builder infos
        let get_result = cache.get_all_builder_infos().await;
        assert!(get_result.is_ok(), "Fetching all builder infos failed");
        assert_eq!(get_result.unwrap().len(), 2, "Incorrect number of builder info entries");
    }

    #[tokio::test]
    async fn test_db_demote_builder() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        let builder_pub_key = BlsPublicKey::try_from([4u8; 48].as_ref()).unwrap();
        let mut builder_info = BuilderInfo::default();
        builder_info.is_optimistic = true;

        // Store some builder info to demote later
        cache.store_builder_info(&builder_pub_key, builder_info.clone()).await.unwrap();

        // Test: Demote the builder
        let demote_result = cache.db_demote_builder(&builder_pub_key).await;
        assert!(demote_result.is_ok(), "Demoting builder failed");

        // Validate: The builder should now be demoted
        let get_result = cache.db_get_builder_info(&builder_pub_key).await.unwrap();
        assert!(!get_result.is_optimistic, "Builder was not demoted");
    }

    #[tokio::test]
    async fn test_save_simulation_result() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        let block_hash = Hash32::try_from([4u8; 32].as_ref()).unwrap();

        // Test: Save a successful simulation result
        let sim_result: Result<(), BlockSimError> = Ok(());
        let result = cache.save_simulation_result(block_hash.clone(), sim_result).await;
        assert!(result.is_ok(), "Saving successful simulation result failed");

        // Validate: The simulation result should not be stored
        let stored_data: Result<Vec<BlockSimErrorDocument>, _> =
            cache.lrange(SIMULATION_ERROR_KEY, 0, -1).await;
        assert!(stored_data.is_ok(), "Fetching stored simulation error data failed");
        assert_eq!(stored_data.unwrap().len(), 0, "Incorrect number of simulation error entries");

        // Test: Save a failed simulation result
        let sim_result: Result<(), BlockSimError> =
            Err(BlockSimError::BlockValidationFailed("INCORRECT NONCE".to_string()));
        let result = cache.save_simulation_result(block_hash.clone(), sim_result).await;
        assert!(result.is_ok(), "Saving failed simulation result failed");

        // Validate: The simulation result should be stored
        let stored_data: Result<Vec<BlockSimErrorDocument>, _> =
            cache.lrange(SIMULATION_ERROR_KEY, 0, -1).await;
        assert!(stored_data.is_ok(), "Fetching stored simulation error data failed");
        assert_eq!(stored_data.unwrap().len(), 1, "Incorrect number of simulation error entries");
    }

    #[tokio::test]
    async fn test_get_bids() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        // Prepare some bids and filters for testing
        let _bids: Vec<SignedBidSubmission> = vec![];
        let filters = BidFilters {
            slot: todo!(),
            cursor: todo!(),
            limit: todo!(),
            block_hash: todo!(),
            block_number: todo!(),
            proposer_pubkey: todo!(),
            builder_pubkey: todo!(),
            order_by: todo!(),
        };

        // Test: Fetch bids based on filters
        let get_result = cache.get_bids(&filters).await;
        assert!(get_result.is_ok(), "Fetching bids based on filters failed");
        let _fetched_bids = get_result.unwrap();
    }

    #[tokio::test]
    async fn test_get_delivered_payloads() {
        let cache = RedisCache::new("redis://127.0.0.1/", Vec::new()).await.unwrap();
        cache.clear_cache().await.unwrap();

        // Prepare some delivered payloads and filters for testing
        let _payloads: Vec<DeliveredPayloadDocument> = vec![];
        let filters = BidFilters {
            slot: todo!(),
            cursor: todo!(),
            limit: todo!(),
            block_hash: todo!(),
            block_number: todo!(),
            proposer_pubkey: todo!(),
            builder_pubkey: todo!(),
            order_by: todo!(),
        };

        // Test: Fetch delivered payloads based on filters
        let get_result = cache.get_delivered_payloads(&filters).await;
        assert!(get_result.is_ok(), "Fetching delivered payloads based on filters failed");
        let _fetched_payloads = get_result.unwrap();
    }
}
