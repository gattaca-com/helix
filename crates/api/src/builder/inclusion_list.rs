use std::sync::Arc;

use axum::{response::IntoResponse, Extension};
use hyper::StatusCode;

use super::api::BuilderApi;
use crate::Api;

impl<A: Api> BuilderApi<A> {
    #[tracing::instrument(skip_all)]
    pub async fn get_inclusion_list(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
    ) -> impl IntoResponse {
        if let Some(inclusion_list) = api.curr_slot_info.inclusion_list() {
            (StatusCode::OK, inclusion_list).into_response()
        } else {
            StatusCode::NOT_FOUND.into_response()
        }
    }
}
