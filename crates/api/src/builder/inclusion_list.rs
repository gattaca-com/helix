use std::sync::Arc;

use axum::{extract::Path, response::IntoResponse, Extension};
use helix_common::utils::get_slot_coordinate;
use hyper::StatusCode;
use tracing::info;

use super::{api::BuilderApi, error::BuilderApiError, InclusionListPathParams};
use crate::Api;

impl<A: Api> BuilderApi<A> {
    #[tracing::instrument(skip_all)]
    pub async fn get_inclusion_list(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        Path(InclusionListPathParams { slot, parent_hash, pub_key }): Path<InclusionListPathParams>,
    ) -> Result<impl IntoResponse, BuilderApiError> {
        info!(requested_slot = %slot, pub_key = %pub_key, parent_hash = %parent_hash,
            "Request for inclusion list."
        );

        let current_list = api.current_inclusion_list.read();

        let Some(current_list) = current_list.as_ref() else {
            info!(requested_slot = %slot, pub_key = %pub_key, parent_hash = %parent_hash,
                "Builder has requested an inclusion list but none has been found in redis."
            );
            return Ok(
                (StatusCode::NOT_FOUND, "No inclusion lists have been generated").into_response()
            );
        };

        let requested_slot_coordinate = get_slot_coordinate(slot as i32, &pub_key, &parent_hash);

        if current_list.slot_coordinate == requested_slot_coordinate {
            Ok((StatusCode::OK, axum::Json(current_list.inclusion_list.clone())).into_response())
        } else {
            info!(requested_slot = %slot, pub_key = %pub_key, parent_hash = %parent_hash,
                "Requested inclusion list for a slot in the past. Current slot: {}",
                api.curr_slot_info.head_slot()
            );
            let response = format!(
                "Requested inclusion list for slot in the past. Current (slot, parent_hash, pubkey): {}, Requested (slot, parent_hash, pubkey): {}",
                current_list.slot_coordinate, requested_slot_coordinate
            );
            Ok((StatusCode::NOT_FOUND, response).into_response())
        }
    }
}
