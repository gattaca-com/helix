use std::sync::Arc;

use axum::{
    extract::{Extension, Query},
    response::IntoResponse,
    Json,
};
use tracing::warn;

use helix_database::DatabaseService;
use helix_common::api::data_api::{
    BuilderBlocksReceivedParams, DeliveredPayloadsResponse, ProposerPayloadDeliveredParams,
    ReceivedBlocksResponse, ValidatorRegistrationParams,
};

use crate::relay_data::error::DataApiError;

pub const PATH_DATA_API: &str = "/relay/v1/data";

pub const PATH_PROPOSER_PAYLOAD_DELIVERED: &str = "/bidtraces/proposer_payload_delivered";
pub const PATH_BUILDER_BIDS_RECEIVED: &str = "/bidtraces/builder_blocks_received";
pub const PATH_VALIDATOR_REGISTRATION: &str = "/validator_registration";

#[derive(Clone)]
pub struct DataApi<DB: DatabaseService> {
    db: Arc<DB>,
}

impl<DB: DatabaseService + 'static> DataApi<DB> {
    pub fn new(db: Arc<DB>) -> Self {
        Self { db }
    }

    /// Implements this API: <https://flashbots.github.io/relay-specs/#/Data/getDeliveredPayloads>
    pub async fn proposer_payload_delivered(
        Extension(data_api): Extension<Arc<DataApi<DB>>>,
        Query(params): Query<ProposerPayloadDeliveredParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        if params.slot.is_some() && params.cursor.is_some() {
            return Err(DataApiError::SlotAndCursor);
        }

        match data_api.db.get_delivered_payloads(&params.into()).await {
            Ok(result) => {
                let response = result
                    .into_iter()
                    .map(|b| b.into())
                    .collect::<Vec<DeliveredPayloadsResponse>>();

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
        Query(params): Query<BuilderBlocksReceivedParams>,
    ) -> Result<impl IntoResponse, DataApiError> {
        if params.slot.is_none()
            && params.block_hash.is_none()
            && params.block_number.is_none()
            && params.builder_pubkey.is_none()
        {
            return Err(DataApiError::MissingFilter);
        }

        if params.limit.is_some() && params.limit.unwrap() > 500 {
            return Err(DataApiError::LimitReached);
        }

        match data_api.db.get_bids(&params.into()).await {
            Ok(result) => {
                let response =
                    result.into_iter().map(|b| b.into()).collect::<Vec<ReceivedBlocksResponse>>();

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
            Ok(result) => {
                Ok(Json(result.registration_info.registration))
            }
            Err(err) => {
                warn!(error=%err, "Failed to get validator registration info");
                Err(DataApiError::InternalServerError)
            }
        }
    }
}
