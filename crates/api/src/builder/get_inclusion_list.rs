use std::sync::Arc;

use axum::{Extension, extract::Path, response::IntoResponse};
use helix_common::api::builder_api::InclusionList;
use hyper::StatusCode;
use tracing::debug;

use super::{InclusionListPathParams, api::BuilderApi, error::BuilderApiError};
use crate::Api;

impl<A: Api> BuilderApi<A> {
    /// Retrieves the inclusion list for the current slot.
    ///
    /// This function accepts a slot number, parent hash and public_key.
    /// 1. Validates that the request's slot is not older than the head slot.
    /// 2. Fetches the current inclusion list from the auctioneer.
    ///
    /// Returns a JSON response containing the inclusion list if found.
    /// Otherwise it returns 404 with an empty body.
    #[tracing::instrument(skip_all, name = "get_inclusion_list", fields(requested_slot = %slot, pub_key = %pub_key, parent_hash = %parent_hash))]
    pub async fn get_inclusion_list(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        Path(InclusionListPathParams { slot, parent_hash, pub_key }): Path<InclusionListPathParams>,
    ) -> Result<impl IntoResponse, BuilderApiError> {
        debug!("New request for inclusion list.");

        let list_with_key = api.local_cache.inclusion_list.read();

        let Some(list_with_key) = list_with_key.as_ref() else {
            debug!(inclusion_lists_enabled = %api.relay_config.inclusion_list.is_some(),
                "Builder has requested an inclusion list but none have been found in the cache."
            );
            return Ok(StatusCode::NOT_FOUND.into_response());
        };

        let (current_list, key) = list_with_key.into();

        if key == &(slot, pub_key, parent_hash) {
            let response_payload = InclusionList::from(current_list);
            Ok((StatusCode::OK, axum::Json(response_payload)).into_response())
        } else {
            debug!(
                "Requested inclusion list for a slot in the past. Current slot: {}",
                api.curr_slot_info.head_slot()
            );

            Ok(StatusCode::NOT_FOUND.into_response())
        }
    }
}
