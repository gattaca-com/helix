use std::sync::Arc;

use axum::{extract::Path, response::IntoResponse, Extension};
use helix_common::{api::builder_api::InclusionList, utils::get_slot_coordinate};
use helix_datastore::types::keys::inclusion_list_key;
use hyper::StatusCode;
use tracing::debug;

use super::{api::BuilderApi, error::BuilderApiError, InclusionListPathParams};
use crate::Api;

impl<A: Api> BuilderApi<A> {
    #[tracing::instrument(skip_all)]
    pub async fn get_inclusion_list(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        Path(InclusionListPathParams { slot, parent_hash, pub_key }): Path<InclusionListPathParams>,
    ) -> Result<impl IntoResponse, BuilderApiError> {
        debug!(requested_slot = %slot, pub_key = %pub_key, parent_hash = %parent_hash,
            "New request for inclusion list."
        );

        let current_list = api.current_inclusion_list.read();

        let Some(current_list) = current_list.as_ref() else {
            debug!(requested_slot = %slot, pub_key = %pub_key, parent_hash = %parent_hash,
                "Builder has requested an inclusion list but none has been found in redis."
            );
            return Ok(StatusCode::NOT_FOUND.into_response());
        };

        let requested_slot_coordinate = get_slot_coordinate(slot as i32, &pub_key, &parent_hash);
        let requested_key = inclusion_list_key(&requested_slot_coordinate);

        if current_list.key == requested_key {
            let response_payload = InclusionList::from(&current_list.inclusion_list);
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
