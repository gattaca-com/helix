use std::sync::Arc;

use axum::{
    extract::{Extension, Query},
    response::IntoResponse,
    Json,
};
use helix_common::{
    api::data_api::{
        BuilderBlocksReceivedParams, DeliveredPayloadsResponse, ProposerPayloadDeliveredParams,
        ReceivedBlocksResponse, ValidatorRegistrationParams,
    },
    ValidatorPreferences,
};
use helix_database::{error::DatabaseError, DatabaseService};
use moka::sync::Cache;
use tracing::warn;

use crate::{relay_data::error::DataApiError, Api};

pub(crate) type BidsCache = Cache<String, Vec<ReceivedBlocksResponse>>;
pub(crate) type DeliveredPayloadsCache = Cache<String, Vec<DeliveredPayloadsResponse>>;

#[derive(Clone)]
pub struct DataApi<A: Api> {
    validator_preferences: Arc<ValidatorPreferences>,
    db: Arc<A::DatabaseService>,
}

impl<A: Api> DataApi<A> {
    pub fn new(
        validator_preferences: Arc<ValidatorPreferences>,
        db: Arc<A::DatabaseService>,
    ) -> Self {
        Self { validator_preferences, db }
    }

    /// Implements this API: <https://flashbots.github.io/relay-specs/#/Data/getDeliveredPayloads>
    pub async fn proposer_payload_delivered(
        Extension(data_api): Extension<Arc<DataApi<A>>>,
        Extension(cache): Extension<Arc<DeliveredPayloadsCache>>,
        Query(mut params): Query<ProposerPayloadDeliveredParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        if params.slot.is_some() && params.cursor.is_some() {
            return Err(DataApiError::SlotAndCursor);
        }

        if params.limit.is_some() && params.limit.unwrap() > 200 {
            return Err(DataApiError::LimitReached { limit: 200 });
        }

        if params.limit.is_none() {
            params.limit = Some(200);
        }

        if params.limit.is_some() && params.limit.unwrap() > 200 {
            return Err(DataApiError::LimitReached { limit: 200 });
        }

        if params.limit.is_none() {
            params.limit = Some(200);
        }

        if params.limit.is_some() && params.limit.unwrap() > 200 {
            return Err(DataApiError::LimitReached { limit: 200 });
        }

        if params.limit.is_none() {
            params.limit = Some(200);
        }

        let cache_key = format!("{:?}", params);

        if let Some(cached_result) = cache.get(&cache_key) {
            return Ok(Json(cached_result));
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
        Extension(data_api): Extension<Arc<DataApi<A>>>,
        Extension(cache): Extension<Arc<BidsCache>>,
        Query(mut params): Query<BuilderBlocksReceivedParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        if params.slot.is_none() &&
            params.block_hash.is_none() &&
            params.block_number.is_none() &&
            params.builder_pubkey.is_none()
        {
            return Err(DataApiError::MissingFilter);
        }

        if params.limit.is_some() && params.limit.unwrap() > 500 {
            return Err(DataApiError::LimitReached { limit: 500 });
        }

        if params.limit.is_none() {
            params.limit = Some(500);
        }

        let cache_key = format!("{:?}", params);

        if let Some(cached_result) = cache.get(&cache_key) {
            return Ok(Json(cached_result));
        }

        match data_api.db.get_bids(&params.into(), data_api.validator_preferences.clone()).await {
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
        Extension(data_api): Extension<Arc<DataApi<A>>>,
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
