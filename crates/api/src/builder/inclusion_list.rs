use std::sync::Arc;

use axum::{extract::Path, response::IntoResponse, Extension};
use helix_common::utils::get_slot_coordinate;
use helix_datastore::Auctioneer;
use hyper::StatusCode;
use tracing::warn;

use super::{api::BuilderApi, error::BuilderApiError, InclusionListPathParams};
use crate::Api;

impl<A: Api> BuilderApi<A> {
    #[tracing::instrument(skip_all)]
    pub async fn get_inclusion_list(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        Path(InclusionListPathParams { slot, parent_hash, pub_key }): Path<InclusionListPathParams>,
    ) -> Result<impl IntoResponse, BuilderApiError> {
        tracing::debug!(slot = %slot, pub_key = %pub_key, parent_hash = %parent_hash, "Request for inclusion list.");

        let Some(current_list) = api.auctioneer.get_current_inclusion_list().await? else {
            return Ok(StatusCode::NOT_FOUND.into_response());
        };

        let current_slot_coordinate = get_slot_coordinate(slot as i32, &pub_key, &parent_hash);

        if current_list.slot_coordinate == current_slot_coordinate {
            Ok((StatusCode::OK, axum::Json(current_list.inclusion_list)).into_response())
        } else {
            warn!("Requesting inclusion list for a slot in the past. Requested slot: {} current slot: {}", slot, api.curr_slot_info.head_slot());
            Ok(StatusCode::NOT_FOUND.into_response())
        }
    }
}
