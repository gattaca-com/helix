use std::sync::Arc;

use axum::{extract::Path, response::IntoResponse, Extension};
use helix_common::{api::builder_api::InclusionList, utils::get_slot_coordinate};
use helix_datastore::types::keys::inclusion_list_key;
use hyper::StatusCode;
use tracing::debug;

use super::{api::BuilderApi, error::BuilderApiError, InclusionListPathParams};
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
    #[tracing::instrument(skip_all)]
    pub async fn get_inclusion_list(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        Path(InclusionListPathParams { slot, parent_hash, pub_key }): Path<InclusionListPathParams>,
    ) -> Result<impl IntoResponse, BuilderApiError> {
        debug!(requested_slot = %slot, pub_key = %pub_key, parent_hash = %parent_hash,
            "New request for inclusion list."
        );

        let list_with_key = api.current_inclusion_list.read();

        let Some(list_with_key) = list_with_key.as_ref() else {
            debug!(requested_slot = %slot, pub_key = %pub_key, parent_hash = %parent_hash,
                "Builder has requested an inclusion list but none has been found in redis."
            );
            return Ok(StatusCode::NOT_FOUND.into_response());
        };

        let (current_list, key) = list_with_key.into();

        let requested_slot_coordinate = get_slot_coordinate(slot, &pub_key, &parent_hash);

        if key == inclusion_list_key(&requested_slot_coordinate) {
            let response_payload = InclusionList::from(current_list);
            Ok((StatusCode::OK, axum::Json(response_payload)).into_response())
        } else {
            debug!(requested_slot = %slot, pub_key = %pub_key, parent_hash = %parent_hash,
                "Requested inclusion list for a slot in the past. Current slot: {}",
                api.curr_slot_info.head_slot()
            );

            Ok(StatusCode::NOT_FOUND.into_response())
        }
    }
}
