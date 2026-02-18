use std::sync::Arc;

use axum::Extension;
use helix_common::{
    self, RequestTimings, SubmissionTrace, api_provider::ApiProvider, utils::extract_request_id,
};
use http::HeaderMap;
use tracing::{error, trace};

use super::api::BuilderApi;
use crate::{
    api::{Api, builder::error::BuilderApiError},
    auctioneer::{InternalBidSubmissionHeader, SubmissionResultSender},
};

impl<A: Api> BuilderApi<A> {
    /// Implements this API: <https://flashbots.github.io/relay-specs/#/Builder/submitBlock>
    #[tracing::instrument(skip_all, err(level = tracing::Level::TRACE),
        fields(
        id = tracing::field::Empty,
        slot = tracing::field::Empty,
        builder_pubkey = tracing::field::Empty,
        builder_id = tracing::field::Empty,
        block_hash = tracing::field::Empty,
    ))]
    pub async fn submit_block(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        Extension(timings): Extension<RequestTimings>,
        headers: HeaderMap,
        body: bytes::Bytes,
    ) -> Result<(), BuilderApiError> {
        let request_id = extract_request_id(&headers);

        tracing::Span::current().record("id", tracing::field::display(request_id));

        trace!("start handler");

        let mut trace = SubmissionTrace::init_from_timings(timings);
        trace.metadata = api.api_provider.get_metadata(&headers);

        let header = InternalBidSubmissionHeader::from_http_headers(request_id, headers);
        let (tx, rx) = tokio::sync::oneshot::channel();
        if api
            .auctioneer_handle
            .block_submission(None, header, body, trace, SubmissionResultSender::OneShot(tx), None)
            .is_err()
        {
            error!("failed sending request to worker");
            return Err(BuilderApiError::InternalError);
        }

        let res = match rx.await {
            Ok((_, res)) => res,
            Err(_) => Err(BuilderApiError::RequestTimeout),
        };

        if let Err(err) = &res &&
            err.should_report()
        {
            error!(%err)
        }

        res
    }
}
