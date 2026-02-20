use std::{sync::Arc, time::Duration};

use axum::{
    Json,
    extract::{Extension, Query},
    response::IntoResponse,
};
use helix_common::{
    ValidatorPreferences,
    api::data_api::{
        BuilderBlocksReceivedParams, DataAdjustmentsParams, DeliveredPayloadsResponse,
        DeliveredPayloadsResponseV2, MergedBlockParams, ProposerHeaderDeliveredParams,
        ProposerPayloadDeliveredParams, ReceivedBlocksResponse, ReceivedBlocksResponseV2,
        ValidatorRegistrationParams,
    },
    metrics,
};
use moka::sync::Cache;
use tracing::{debug, warn};

use crate::{
    api::relay_data::{
        BuilderBlocksReceivedStats, ProposerPayloadDeliveredStats, error::DataApiError,
    },
    database::{error::DatabaseError, postgres::postgres_db_service::PostgresDatabaseService},
};

pub(crate) type BidsCache = Cache<BuilderBlocksReceivedParams, Vec<ReceivedBlocksResponse>>;
pub(crate) type BidsCacheV2 = Cache<BuilderBlocksReceivedParams, Vec<ReceivedBlocksResponseV2>>;
pub(crate) type DeliveredPayloadsCache =
    Cache<ProposerPayloadDeliveredParams, Vec<DeliveredPayloadsResponse>>;
pub(crate) type DeliveredPayloadsCacheV2 =
    Cache<ProposerPayloadDeliveredParams, Vec<DeliveredPayloadsResponseV2>>;

#[derive(Clone)]
pub struct DataApi {
    validator_preferences: Arc<ValidatorPreferences>,
    db: Arc<PostgresDatabaseService>,
    payload_delivered_stats: ProposerPayloadDeliveredStats,
    payload_delivered_stats_v2: ProposerPayloadDeliveredStats,
    builder_blocks_received_stats: BuilderBlocksReceivedStats,
    builder_blocks_received_stats_v2: BuilderBlocksReceivedStats,
}

impl DataApi {
    pub fn new(
        validator_preferences: Arc<ValidatorPreferences>,
        db: Arc<PostgresDatabaseService>,
    ) -> Self {
        let payload_delivered_stats = ProposerPayloadDeliveredStats::default();
        let builder_blocks_received_stats = BuilderBlocksReceivedStats::default();

        let delivered = payload_delivered_stats.clone();
        let blocks = builder_blocks_received_stats.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                delivered.maybe_log_reset();
                blocks.maybe_log_reset();
            }
        });

        Self {
            validator_preferences,
            db,
            payload_delivered_stats: Default::default(),
            payload_delivered_stats_v2: Default::default(),
            builder_blocks_received_stats: Default::default(),
            builder_blocks_received_stats_v2: Default::default(),
        }
    }

    /// Implements this API: <https://flashbots.github.io/relay-specs/#/Data/getDeliveredPayloads>
    pub async fn proposer_payload_delivered(
        Extension(data_api): Extension<Arc<DataApi>>,
        Extension(cache): Extension<DeliveredPayloadsCache>,
        Query(mut params): Query<ProposerPayloadDeliveredParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        if params.slot.is_some() && params.cursor.is_some() {
            return Err(DataApiError::SlotAndCursor);
        }

        if params.limit.is_some() && params.limit.unwrap() > 200 {
            return Err(DataApiError::LimitReached { limit: 200 });
        }
        data_api.payload_delivered_stats.record_total(&params);

        if params.limit.is_none() {
            params.limit = Some(200);
        }

        if let Some(cached_result) = cache.get(&params) {
            data_api.payload_delivered_stats.record_cache_hit();
            metrics::delivered_payloads_cache_hit();
            return Ok(Json(cached_result));
        }

        debug!(?params, ?data_api.validator_preferences, "fetching payloads");

        match data_api
            .db
            .get_delivered_payloads(&(&params).into(), data_api.validator_preferences.clone())
            .await
        {
            Ok(result) => {
                debug!(?result, "payloads fetched");
                let mut seen = std::collections::HashSet::with_capacity(result.len());
                let response = result
                    .into_iter()
                    .filter(|b| seen.insert(b.bid_trace.block_hash))
                    .map(|b| b.into())
                    .collect::<Vec<DeliveredPayloadsResponse>>();

                cache.insert(params, response.clone());

                Ok(Json(response))
            }
            Err(err) => {
                warn!(error=%err, "Failed to fetch delivered payloads");
                Err(DataApiError::InternalServerError)
            }
        }
    }

    pub async fn proposer_payload_delivered_v2(
        Extension(data_api): Extension<Arc<DataApi>>,
        Extension(cache): Extension<DeliveredPayloadsCacheV2>,
        Query(mut params): Query<ProposerPayloadDeliveredParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        if params.slot.is_some() && params.cursor.is_some() {
            return Err(DataApiError::SlotAndCursor);
        }

        if params.limit.is_some() && params.limit.unwrap() > 200 {
            return Err(DataApiError::LimitReached { limit: 200 });
        }
        data_api.payload_delivered_stats_v2.record_total(&params);

        if params.limit.is_none() {
            params.limit = Some(200);
        }

        if let Some(cached_result) = cache.get(&params) {
            data_api.payload_delivered_stats_v2.record_cache_hit();
            metrics::delivered_payloads_cache_hit();
            return Ok(Json(cached_result));
        }

        debug!(?params, ?data_api.validator_preferences, "fetching payloads v2");

        match data_api
            .db
            .get_delivered_payloads(&(&params).into(), data_api.validator_preferences.clone())
            .await
        {
            Ok(result) => {
                debug!(?result, "payloads fetched v2");
                let response = result
                    .into_iter()
                    .map(|b| b.into())
                    .collect::<Vec<DeliveredPayloadsResponseV2>>();

                cache.insert(params, response.clone());

                Ok(Json(response))
            }
            Err(err) => {
                warn!(error=%err, "Failed to fetch delivered payloads v2");
                Err(DataApiError::InternalServerError)
            }
        }
    }
    /// Implements this API: <https://flashbots.github.io/relay-specs/#/Data/getReceivedBids>
    pub async fn builder_bids_received(
        Extension(data_api): Extension<Arc<DataApi>>,
        Extension(cache): Extension<BidsCache>,
        Query(mut params): Query<BuilderBlocksReceivedParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        if params.slot.is_none() &&
            params.block_hash.is_none() &&
            params.block_number.is_none() &&
            params.builder_pubkey.is_none()
        {
            return Err(DataApiError::MissingFilter);
        }

        if params.limit.is_some() && params.limit.unwrap() > 500 && params.slot.is_none() {
            return Err(DataApiError::LimitReached { limit: 500 });
        }
        data_api.builder_blocks_received_stats.record_total(&params);

        // No need to apply limit for slot/block number based queries as the result set is already
        // limited and returning less than the full set here would be misleading.
        if params.limit.is_none() && params.slot.is_none() && params.block_number.is_none() {
            params.limit = Some(500);
        }

        if let Some(cached_result) = cache.get(&params) {
            data_api.builder_blocks_received_stats.record_cache_hit();
            metrics::bids_cache_hit();
            return Ok(Json(cached_result));
        }

        match data_api.db.get_bids(&(&params).into(), data_api.validator_preferences.clone()).await
        {
            Ok(result) => {
                let mut seen = std::collections::HashSet::with_capacity(result.len());
                let response = result
                    .into_iter()
                    .filter(|b| seen.insert(b.bid_trace.block_hash))
                    .map(|b| b.into())
                    .collect::<Vec<ReceivedBlocksResponse>>();

                cache.insert(params, response.clone());

                Ok(Json(response))
            }
            Err(err) => {
                warn!(error=%err, "Failed to fetch bids");
                Err(DataApiError::InternalServerError)
            }
        }
    }

    pub async fn builder_bids_received_v2(
        Extension(data_api): Extension<Arc<DataApi>>,
        Extension(cache): Extension<BidsCacheV2>,
        Query(mut params): Query<BuilderBlocksReceivedParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        if params.slot.is_none() &&
            params.block_hash.is_none() &&
            params.block_number.is_none() &&
            params.builder_pubkey.is_none()
        {
            return Err(DataApiError::MissingFilter);
        }

        if params.limit.is_some() && params.limit.unwrap() > 500 && params.slot.is_none() {
            return Err(DataApiError::LimitReached { limit: 500 });
        }
        data_api.builder_blocks_received_stats_v2.record_total(&params);

        // No need to apply limit for slot/block number based queries as the result set is already
        // limited and returning less than the full set here would be misleading.
        if params.limit.is_none() && params.slot.is_none() && params.block_number.is_none() {
            params.limit = Some(500);
        }

        if let Some(cached_result) = cache.get(&params) {
            data_api.builder_blocks_received_stats_v2.record_cache_hit();
            metrics::bids_cache_hit();
            return Ok(Json(cached_result));
        }

        match data_api.db.get_bids(&(&params).into(), data_api.validator_preferences.clone()).await
        {
            Ok(result) => {
                let response =
                    result.into_iter().map(|b| b.into()).collect::<Vec<ReceivedBlocksResponseV2>>();

                cache.insert(params, response.clone());

                Ok(Json(response))
            }
            Err(err) => {
                warn!(error=%err, "Failed to fetch bids");
                Err(DataApiError::InternalServerError)
            }
        }
    }

    /// Implements this API: <https://flashbots.github.io/relay-specs/#/Data/getValidatorRegistration>
    pub async fn validator_registration(
        Extension(data_api): Extension<Arc<DataApi>>,
        Query(params): Query<ValidatorRegistrationParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        match data_api.db.get_validator_registration(&params.pubkey).await {
            Ok(result) => Ok(Json(result.registration_info.registration)),
            Err(err) => match err {
                DatabaseError::ValidatorRegistrationNotFound => {
                    warn!("Validator registration not found");
                    Err(DataApiError::ValidatorRegistrationNotFound { pubkey: params.pubkey })
                }
                _ => {
                    warn!(error=%err, "Failed to get validator registration info");
                    Err(DataApiError::InternalServerError)
                }
            },
        }
    }

    // Implements this API: https://docs.ultrasound.money/builders/bid-adjustment#data-api
    // relay/v1/data/adjustments?slot=123
    pub async fn data_adjustments(
        Extension(data_api): Extension<Arc<DataApi>>,
        Query(params): Query<DataAdjustmentsParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        match data_api.db.get_block_adjustments_for_slot(params.slot).await {
            Ok(result) => Ok(Json(result)),
            Err(err) => {
                warn!(%err, "Failed to get slot adjustments info");
                Err(DataApiError::InternalServerError)
            }
        }
    }

    pub async fn proposer_header_delivered(
        Extension(data_api): Extension<Arc<DataApi>>,
        Query(mut params): Query<ProposerHeaderDeliveredParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        if params.block_number.is_some() {
            return Err(DataApiError::BlockNumberNotSupported);
        }
        if params.order_by.is_some() {
            return Err(DataApiError::OrderByNotSupported);
        }
        if params.builder_pubkey.is_some() {
            return Err(DataApiError::BuilderPubkeyNotSupported);
        }
        if params.limit.map(|l| l > 200).unwrap_or(false) {
            return Err(DataApiError::LimitReached { limit: 200 });
        }
        if params.limit.is_none() {
            params.limit = Some(200);
        }

        match data_api.db.get_proposer_header_delivered(&params).await {
            Ok(result) => Ok(Json(result)),
            Err(err) => {
                warn!(error=%err, "Failed to fetch proposer header delivered");
                Err(DataApiError::InternalServerError)
            }
        }
    }

    pub async fn merged_blocks(
        Extension(data_api): Extension<Arc<DataApi>>,
        Query(params): Query<MergedBlockParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        match data_api.db.get_merged_blocks_for_slot(params.slot).await {
            Ok(result) => Ok(Json(result)),
            Err(err) => {
                warn!(%err, "Failed to get merged blocks info");
                Err(DataApiError::InternalServerError)
            }
        }
    }
}
