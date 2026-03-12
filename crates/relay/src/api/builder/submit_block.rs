use std::{sync::Arc, time::Duration};

use axum::{
    Extension,
    response::{IntoResponse, Response},
};
use flux::{
    spine::SpineProducers,
    timing::{IngestionTime, Nanos},
};
use flux_utils::ArrayStr;
use helix_common::{
    self, RequestTimings, SubmissionTrace, api_provider::ApiProvider,
    metrics::SUB_CLIENT_TO_SERVER_LATENCY, utils::extract_request_id,
};
use http::{HeaderMap, StatusCode};
use tokio::time::timeout;
use tracing::trace;

use super::api::BuilderApi;
use crate::{
    api::{Api, FutureBidSubmissionResult, builder::error::BuilderApiError},
    auctioneer::{InternalBidSubmissionHeader, SubmissionRef},
    spine::messages::NewBidSubmission,
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

        match api.submissions.write(body.len(), |buf| buf.copy_from_slice(&body)) {
            Ok(dref) => api.producer.produce_with_ingestion(
                NewBidSubmission {
                    dref,
                    header,
                    submission_ref: SubmissionRef::Http(future_ix),
                    trace,
                    expected_pubkey: None,
                },
                IngestionTime::now(),
            ),
            Err(e) => {
                tracing::error!("failed to write bid submission into dcache: {e}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }

        let Some(future) = api.future_results.get(future_ix) else {
            tracing::error!("failed to find future response in the shared vec");
            return BuilderApiError::InternalError.into_response();
        };

        if let Ok(result) =
            timeout(Duration::from_secs(3), FutureBidSubmissionResult::wait(future)).await
        {
            if result.tcp_status.is_okay() {
                StatusCode::OK.into_response()
            } else {
                if result.should_report {
                    tracing::error!(err = result.error_msg.as_str());
                }
                (result.http_status, result.error_msg.to_string()).into_response()
            }
        } else {
            tracing::error!("timeout while waiting for bid submission processing respopnse");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
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
