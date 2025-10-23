use std::{sync::Arc, time::Duration};

use axum::{
    Json,
    extract::{Extension, Query},
    response::IntoResponse,
};
use helix_common::{
    ValidatorPreferences,
    api::data_api::{
        BuilderBlocksReceivedParams, DeliveredPayloadsResponse, ProposerPayloadDeliveredParams,
        ReceivedBlocksResponse, ValidatorRegistrationParams,
    },
    metrics,
};
use moka::sync::Cache;
use tracing::warn;

use crate::{
    api::relay_data::{
        BuilderBlocksReceivedStats, ProposerPayloadDeliveredStats, error::DataApiError,
    },
    database::{error::DatabaseError, postgres::postgres_db_service::PostgresDatabaseService},
};

pub(crate) type BidsCache = Cache<BuilderBlocksReceivedParams, Vec<ReceivedBlocksResponse>>;
pub(crate) type DeliveredPayloadsCache =
    Cache<ProposerPayloadDeliveredParams, Vec<DeliveredPayloadsResponse>>;

#[derive(Clone)]
pub struct DataApi {
    validator_preferences: Arc<ValidatorPreferences>,
    db: Arc<PostgresDatabaseService>,
    payload_delivered_stats: ProposerPayloadDeliveredStats,
    builder_blocks_received_stats: BuilderBlocksReceivedStats,
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
            builder_blocks_received_stats: Default::default(),
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

        match data_api
            .db
            .get_delivered_payloads(&(&params).into(), data_api.validator_preferences.clone())
            .await
        {
            Ok(result) => {
                let response = result
                    .into_iter()
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

        if params.limit.is_none() && params.slot.is_none() {
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
                let response =
                    result.into_iter().map(|b| b.into()).collect::<Vec<ReceivedBlocksResponse>>();

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
}
