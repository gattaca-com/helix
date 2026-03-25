use std::sync::Arc;

use axum::{Extension, http::StatusCode, response::IntoResponse};
use crossbeam_channel::Sender;
use flux::spine::StandaloneDCacheProducer;
use flux_utils::SharedVector;
use helix_common::{RelayConfig, local_cache::LocalCache};
use helix_database::handle::DbHandle;

use crate::{
    api::{Api, FutureBidSubmissionResult, extract::raw_web_socket::RawWebSocket},
    auctioneer::AuctioneerHandle,
    housekeeper::CurrentSlotInfo,
    spine::messages::NewBidSubmission,
};

pub struct BuilderApi<A: Api> {
    pub local_cache: Arc<LocalCache>,
    pub db: DbHandle,
    pub curr_slot_info: CurrentSlotInfo,
    pub relay_config: Arc<RelayConfig>,
    pub auctioneer_handle: AuctioneerHandle,
    pub api_provider: Arc<A::ApiProvider>,
    pub producer: StandaloneDCacheProducer<NewBidSubmission>,
    pub future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
    pub web_socket_connections: Sender<RawWebSocket>,
}

impl<A: Api> BuilderApi<A> {
    pub fn new(
        local_cache: Arc<LocalCache>,
        db: DbHandle,
        relay_config: RelayConfig,
        curr_slot_info: CurrentSlotInfo,
        auctioneer_handle: AuctioneerHandle,
        api_provider: Arc<A::ApiProvider>,
        producer: StandaloneDCacheProducer<NewBidSubmission>,
        future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
        web_socket_connections: Sender<RawWebSocket>,
    ) -> Self {
        Self {
            local_cache,
            db,
            relay_config: Arc::new(relay_config),
            curr_slot_info,
            auctioneer_handle,
            api_provider,
            producer,
            future_results,
            web_socket_connections,
        }
    }

    /// Implements this API: <https://flashbots.github.io/relay-specs/#/Builder/getValidators>
    pub async fn get_validators(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
    ) -> impl IntoResponse {
        if let Some(duty_bytes) = api.curr_slot_info.proposer_duties_response() {
            (StatusCode::OK, duty_bytes.0).into_response()
        } else {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
