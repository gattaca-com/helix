use std::sync::Arc;

use axum::Extension;
use helix_common::{
    self, metadata_provider::MetadataProvider, utils::extract_request_id, RequestTimings,
    SubmissionTrace,
};
use http::HeaderMap;
use tracing::error;

use super::api::BuilderApi;
use crate::{builder::error::BuilderApiError, Api};

impl<A: Api> BuilderApi<A> {
    /// Implements this API: <https://flashbots.github.io/relay-specs/#/Builder/submitBlock>
    #[tracing::instrument(skip_all, fields(
        id =% extract_request_id(&headers),
        slot = tracing::field::Empty, // submission slot
        builder_pubkey = tracing::field::Empty,
        builder_id = tracing::field::Empty,
        block_hash = tracing::field::Empty,
    ), err)]
    pub async fn submit_block(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        Extension(timings): Extension<RequestTimings>,
        headers: HeaderMap,
        body: bytes::Bytes,
    ) -> Result<(), BuilderApiError> {
        let mut trace = SubmissionTrace::init_from_timings(timings);
        trace.metadata = api.metadata_provider.get_metadata(&headers);

        let Ok(rx) = api.auctioneer_handle.block_submission(headers, body, trace) else {
            error!("failed sending request to worker");
            return Err(BuilderApiError::InternalError)
        };

        match rx.await {
            Ok(res) => res,
            Err(_) => Err(BuilderApiError::RequestTimeout),
        }
    }
}
