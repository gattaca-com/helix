use std::sync::Arc;

use axum::{extract::Path, response::IntoResponse, Extension};
use helix_datastore::{redis::redis_cache::InclusionListWithKey, Auctioneer};
use hyper::StatusCode;
use tracing::{info, warn};

use super::{api::BuilderApi, InclusionListPathParams};
use crate::Api;

impl<A: Api> BuilderApi<A> {
    // #[tracing::instrument(skip_all)]
    pub async fn get_inclusion_list(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        Path(InclusionListPathParams { slot, parent_hash, pub_key }): Path<InclusionListPathParams>,
    ) -> impl IntoResponse {
        // let slot_info = api.curr_slot_info.slot_info().1;
        // let Some(slot_info) = slot_info else {
        //     return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        // };

        tracing::error!("slot: {slot}, pub_key: {pub_key}, parent hash{parent_hash}");

        // let current_pub_key = slot_info.entry.registration.message.pubkey;
        // let current_slot = slot_info.slot.as_u64();

        // if slot != current_slot
        // /*|| pub_key != current_pub_key.to_string()*/
        // {
        //     return StatusCode::NOT_FOUND.into_response();
        // }

        let inclusion_list_with_key =
            api.auctioneer.get_current_inclusion_list(slot as i32, &parent_hash).await;

        match inclusion_list_with_key {
            Ok(Some(InclusionListWithKey { inclusion_list, slot_coordinate })) => {
                if slot_coordinate == format!("{slot}") {
                    (StatusCode::OK, axum::Json(inclusion_list)).into_response()
                } else {
                    warn!("Requesting inclusion list for a slot in the past. requested slot: {} current slot: {}", slot, api.curr_slot_info.head_slot());
                    StatusCode::NOT_FOUND.into_response()
                }
            }
            Ok(None) => StatusCode::NOT_FOUND.into_response(),
            Err(err) => {
                tracing::error!("500 {err}");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}
