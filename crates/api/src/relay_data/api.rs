use std::sync::Arc;

use axum::{
    extract::{Extension, Query},
    response::IntoResponse,
    Json,
};
use moka::sync::Cache;
use tracing::warn;

use helix_common::{
    api::data_api::{
        BuilderBlocksReceivedParams, DeliveredPayloadsResponse, ProposerPayloadDeliveredParams,
        ReceivedBlocksResponse, ValidatorRegistrationParams,
    },
    ValidatorPreferences,
};
use helix_database::DatabaseService;

use crate::relay_data::error::DataApiError;

pub(crate) const PATH_DATA_API: &str = "/relay/v1/data";

pub(crate) const PATH_PROPOSER_PAYLOAD_DELIVERED: &str = "/bidtraces/proposer_payload_delivered";
pub(crate) const PATH_BUILDER_BIDS_RECEIVED: &str = "/bidtraces/builder_blocks_received";
pub(crate) const PATH_VALIDATOR_REGISTRATION: &str = "/validator_registration";

pub(crate) type BidsCache = Cache<String, Vec<ReceivedBlocksResponse>>;
pub(crate) type DeliveredPayloadsCache = Cache<String, Vec<DeliveredPayloadsResponse>>;

#[derive(Clone)]
pub struct DataApi<DB: DatabaseService> {
    validator_preferences: Arc<ValidatorPreferences>,
    db: Arc<DB>,
}

impl<DB: DatabaseService + 'static> DataApi<DB> {
    pub fn new(validator_preferences: Arc<ValidatorPreferences>, db: Arc<DB>) -> Self {
        Self { validator_preferences, db }
    }

    /// Implements this API: <https://flashbots.github.io/relay-specs/#/Data/getDeliveredPayloads>
    pub async fn proposer_payload_delivered(
        Extension(data_api): Extension<Arc<DataApi<DB>>>,
        Extension(cache): Extension<Arc<DeliveredPayloadsCache>>,
        Query(params): Query<ProposerPayloadDeliveredParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        if params.slot.is_some() && params.cursor.is_some() {
            return Err(DataApiError::SlotAndCursor)
        }

        let cache_key = format!("{:?}", params);

        if let Some(cached_result) = cache.get(&cache_key) {
            return Ok(Json(cached_result))
        }

        match data_api
            .db
            .get_delivered_payloads(&params.into(), data_api.validator_preferences.clone())
            .await
        {
            Ok(result) => {
                let response = result
                    .into_iter()
                    .map(|b| b.into())
                    .collect::<Vec<DeliveredPayloadsResponse>>();

                cache.insert(cache_key, response.clone());

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
        Extension(data_api): Extension<Arc<DataApi<DB>>>,
        Extension(cache): Extension<Arc<BidsCache>>,
        Query(params): Query<BuilderBlocksReceivedParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        if params.slot.is_none() &&
            params.block_hash.is_none() &&
            params.block_number.is_none() &&
            params.builder_pubkey.is_none()
        {
            return Err(DataApiError::MissingFilter)
        }

        if params.limit.is_some() && params.limit.unwrap() > 500 {
            return Err(DataApiError::LimitReached)
        }

        let cache_key = format!("{:?}", params);

        if let Some(cached_result) = cache.get(&cache_key) {
            return Ok(Json(cached_result))
        }

        match data_api.db.get_bids(&params.into()).await {
            Ok(result) => {
                let response =
                    result.into_iter().map(|b| b.into()).collect::<Vec<ReceivedBlocksResponse>>();

                cache.insert(cache_key, response.clone());

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
        Extension(data_api): Extension<Arc<DataApi<DB>>>,
        Query(params): Query<ValidatorRegistrationParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        match data_api.db.get_validator_registration(params.pubkey).await {
            Ok(result) => Ok(Json(result.registration_info.registration)),
            Err(err) => {
                warn!(error=%err, "Failed to get validator registration info");
                Err(DataApiError::InternalServerError)
            }
        }
    }
}
