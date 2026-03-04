use std::{sync::Arc, time::Instant};

use axum::{
    Extension,
    response::{IntoResponse, Response},
};
use flux::{spine::SpineProducers, timing::Nanos};
use flux_utils::ArrayStr;
use helix_common::{
    self, RequestTimings, SubmissionTrace, api_provider::ApiProvider,
    metrics::SUB_CLIENT_TO_SERVER_LATENCY, utils::extract_request_id,
};
use http::{HeaderMap, StatusCode};
use tracing::trace;

use super::api::BuilderApi;
use crate::{
    api::{
        Api, builder::error::BuilderApiError, submission_results_fanout::FutureBidSubmissionResult,
    },
    auctioneer::{InternalBidSubmission, InternalBidSubmissionHeader, SubmissionRef},
    spine::messages::NewBidSubmissionIx,
};

const HEADER_SEND_TS: &str = "x-send-ts";

impl<A: Api> BuilderApi<A> {
    /// Implements this API: <https://flashbots.github.io/relay-specs/#/Builder/submitBlock>
    #[tracing::instrument(skip_all,
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
    ) -> Response {
        let id = extract_request_id(&headers);

        tracing::Span::current().record("id", tracing::field::display(id));

        trace!("start handler");

        let mut trace = SubmissionTrace::init_from_timings(timings);
        trace.metadata =
            api.api_provider.get_metadata(&headers).map(|s| ArrayStr::from_str_truncate(&s));

        observe_client_to_server_latency(&headers, trace.receive_ns.0);

        let header = InternalBidSubmissionHeader::from_http_headers(id, headers);

        let future_ix = api.future_results.push(FutureBidSubmissionResult::new());
        let internal_bid = InternalBidSubmission {
            header,
            submission_ref: SubmissionRef::Http(future_ix),
            trace,
            body,
            span: tracing::Span::current(),
            sent_at: Instant::now(),
            expected_pubkey: None,
        };
        let ix = api.submissions.push(internal_bid);
        api.producer.produce(NewBidSubmissionIx { ix });

        let Some(future) = api.future_results.get(future_ix) else {
            tracing::error!("failed to find future response in the shared vec");
            return BuilderApiError::InternalError.into_response();
        };

        let result_ix = FutureBidSubmissionResult::wait(future).await;
        let Some(result) = api.submission_results.get(result_ix) else {
            tracing::error!("submission result missing from shared vec");
            return BuilderApiError::InternalError.into_response();
        };
        match &result.result {
            Ok(()) => StatusCode::OK.into_response(),
            Err(err) => {
                if err.should_report() {
                    tracing::error!(%err);
                }
                err.into_response()
            }
        }
    }
}

fn observe_client_to_server_latency(headers: &HeaderMap, receive_ns: u64) {
    if let Some(send_ts) = headers.get(HEADER_SEND_TS) &&
        let Some(send_ts) = send_ts.to_str().ok().and_then(Nanos::from_rfc3339)
    {
        SUB_CLIENT_TO_SERVER_LATENCY
            .with_label_values(&["http"])
            .observe((receive_ns.saturating_sub(send_ts.0) / 1000) as f64);
    }
}
