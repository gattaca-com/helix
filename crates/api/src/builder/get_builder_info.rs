use std::sync::Arc;

use axum::{extract::Path, response::IntoResponse, Extension};
use helix_database::DatabaseService;
use helix_types::BlsPublicKeyBytes;
use http::HeaderMap;
use hyper::StatusCode;
use tracing::debug;

use super::{api::BuilderApi, error::BuilderApiError};
use crate::{Api, HEADER_API_KEY};

impl<A: Api> BuilderApi<A> {
    /// Retrieves the builder info for a given builder public key.
    ///
    /// This function accepts a `pub_key` and:
    /// 1. Validates the request is authenticated.
    /// 2. Looks up the builder info in the database.
    /// 3. Returns the builder info if found, otherwise 404.
    ///
    /// Returns a JSON response containing fields like `collateral` and `is_optimistic`.
    #[tracing::instrument(skip_all, name = "get_builder_info", fields(pub_key = %pub_key))]
    pub async fn get_builder_info(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        Path(pub_key): Path<BlsPublicKeyBytes>,
        headers: HeaderMap,
    ) -> Result<impl IntoResponse, BuilderApiError> {
        debug!(pub_key = ?pub_key, "New request for builder info.");

        let Some(api_key) = headers.get(HEADER_API_KEY) else {
            return Err(BuilderApiError::InvalidApiKey);
        };

        if !api.auctioneer.validate_api_key(api_key, &pub_key) {
            return Err(BuilderApiError::InvalidApiKey);
        }

        let builder_info =
            api.db.get_builder_info(&pub_key).await.map_err(BuilderApiError::DatabaseError)?;

        match builder_info {
            Some(info) => Ok((StatusCode::OK, axum::Json(info)).into_response()),
            None => Ok(StatusCode::NOT_FOUND.into_response()),
        }
    }
}
