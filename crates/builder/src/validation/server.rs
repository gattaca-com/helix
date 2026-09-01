use std::{net::SocketAddr, sync::Arc, time::Instant};

use alloy_primitives::Address;
use alloy_rpc_types::beacon::relay::SignedBidSubmissionV5;
use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use dashmap::DashSet;
use helix_common::{
    api::builder_api::MAX_PAYLOAD_LENGTH,
    blacklist::{DisallowListPayload, changed_disallow_hash},
    decoder::{DecoderError, SubmissionDecoder, SubmissionDecoderParams},
    simulator::{SszMergedValidationRequest, SszValidationRequest, SszValidationResponse},
};
use helix_types::{ForkName, Submission};
use ssz::{Decode, Encode};
use tokio::{net::TcpListener, sync::Semaphore, time};
use tracing::{error, info, warn};

use crate::{
    engine::convert::eblobs,
    metrics,
    validation::{BlockValidator, error::ValidationError},
};

#[derive(Clone)]
struct ServerState {
    validator: BlockValidator,
    permits: Arc<Semaphore>,
}

pub fn router(validator: BlockValidator, max_concurrent: usize) -> Router {
    Router::new()
        .route("/validate", post(validate))
        .route("/validate_merged", post(validate_merged))
        .layer(DefaultBodyLimit::max(MAX_PAYLOAD_LENGTH))
        .with_state(ServerState {
            validator,
            permits: Arc::new(Semaphore::new(max_concurrent.max(1))),
        })
}

pub async fn run(validator: BlockValidator, addr: SocketAddr, max_concurrent: usize) {
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            error!(%err, %addr, "failed to bind the validation server");
            return;
        }
    };
    info!(%addr, "Validation server listening");
    if let Err(err) = axum::serve(listener, router(validator, max_concurrent)).await {
        error!(%err, "validation server exited");
    }
}

/// Forks this validator understands. A submission from any other fork is
/// refused rather than validated under the wrong rules.
fn supported_fork(params: &Option<SubmissionDecoderParams>) -> bool {
    // No params means raw Fulu-shaped SSZ bytes.
    params.as_ref().is_none_or(|params| matches!(params.fork_name, ForkName::Fulu))
}

/// 501, not 400: the relay maps a 400 body to `BlockValidationFailed`, which
/// demotes the builder. This is helix's own limitation, not the builder's.
fn unsupported_fork(params: &Option<SubmissionDecoderParams>) -> Response {
    let fork = params.as_ref().map(|params| params.fork_name);
    warn!(?fork, "refusing a submission from an unsupported fork");
    (StatusCode::NOT_IMPLEMENTED, format!("unsupported fork: {fork:?}")).into_response()
}

/// A dehydrated submission needs transactions this simulator does not cache.
/// The relay answers a 424 by retrying with full SSZ bytes.
fn decode_submission(
    params: Option<SubmissionDecoderParams>,
    bytes: &[u8],
) -> Result<Option<SignedBidSubmissionV5>, DecoderError> {
    match params {
        Some(params) => {
            let mut buf = Vec::new();
            let (submission, _, _) = SubmissionDecoder::new(&params).decode(bytes, &mut buf)?;
            match submission {
                Submission::Full(submission) => Ok(Some(submission.into())),
                Submission::Dehydrated(_) => Ok(None),
            }
        }
        None => Ok(Some(SignedBidSubmissionV5::from_ssz_bytes(bytes)?)),
    }
}

async fn validate(State(state): State<ServerState>, body: axum::body::Bytes) -> Response {
    let start = Instant::now();
    let request = match SszValidationRequest::from_ssz_bytes(&body) {
        Ok(request) => request,
        Err(err) => {
            return finish("validate", "bad_request", start, bad_request(format!("{err:?}")))
        }
    };
    if !supported_fork(&request.decoder_params) {
        return finish(
            "validate",
            "unsupported_fork",
            start,
            unsupported_fork(&request.decoder_params),
        );
    }
    let t = metrics::sim_lap("decode_request", start);
    let submission = match decode_submission(request.decoder_params, &request.signed_bid_submission)
    {
        Ok(Some(submission)) => submission,
        Ok(None) => {
            return finish(
                "validate",
                "dehydrated",
                start,
                StatusCode::FAILED_DEPENDENCY.into_response(),
            );
        }
        Err(err) => {
            return finish("validate", "bad_submission", start, bad_request(err.to_string()))
        }
    };
    metrics::sim_lap("decode_submission", t);

    run_validation(state, "validate", start, move |validator| {
        validator.validate(
            &submission.execution_payload,
            &submission.message,
            request.parent_beacon_block_root,
            &submission.execution_requests,
            &eblobs(&submission.blobs_bundle),
            request.apply_blacklist,
        )
    })
    .await
}

async fn validate_merged(State(state): State<ServerState>, body: axum::body::Bytes) -> Response {
    let start = Instant::now();
    let request = match SszMergedValidationRequest::from_ssz_bytes(&body) {
        Ok(request) => request,
        Err(err) => {
            return finish("validate_merged", "bad_request", start, bad_request(format!("{err:?}")))
        }
    };
    if !supported_fork(&request.decoder_params) {
        return finish(
            "validate_merged",
            "unsupported_fork",
            start,
            unsupported_fork(&request.decoder_params),
        );
    }
    let t = metrics::sim_lap("decode_request", start);
    let submission = match decode_submission(request.decoder_params, &request.signed_bid_submission)
    {
        Ok(Some(submission)) => submission,
        Ok(None) => {
            return finish(
                "validate_merged",
                "dehydrated",
                start,
                StatusCode::FAILED_DEPENDENCY.into_response(),
            );
        }
        Err(err) => {
            return finish("validate_merged", "bad_submission", start, bad_request(err.to_string()))
        }
    };
    metrics::sim_lap("decode_submission", t);

    run_validation(state, "validate_merged", start, move |validator| {
        validator.validate_merged(
            &submission.execution_payload,
            &submission.message,
            request.parent_beacon_block_root,
            &submission.execution_requests,
            &eblobs(&submission.blobs_bundle),
            request.apply_blacklist,
            request.base_payment_tx_index,
        )
    })
    .await
}

/// Validation is CPU-bound and synchronous, so it runs on a blocking thread.
/// The semaphore caps how many run at once. A pass answers with the SSZ
/// [`SszValidationResponse`].
async fn run_validation<F>(
    state: ServerState,
    route: &'static str,
    start: Instant,
    validate: F,
) -> Response
where
    F: FnOnce(&BlockValidator) -> Result<crate::validation::ExecutedBlock, ValidationError>
        + Send
        + 'static,
{
    let t = Instant::now();
    let Ok(_permit) = state.permits.clone().acquire_owned().await else {
        let response = bad_request("validation server is shutting down".to_string());
        return finish(route, "shutting_down", start, response);
    };
    let queued = metrics::sim_lap("permit_wait", t);
    let validator = state.validator.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _in_flight = metrics::SimInFlight::enter();
        metrics::sim_lap("blocking_wait", queued);
        let executed = validate(&validator)?;
        let t = Instant::now();
        let body = SszValidationResponse { txs: executed.tx_details }.as_ssz_bytes();
        metrics::sim_lap("encode_response", t);
        Ok::<_, ValidationError>(body)
    })
    .await;

    match result {
        Ok(Ok(body)) => finish(route, "ok", start, body.into_response()),
        Ok(Err(err)) => finish(route, err.metric_label(), start, bad_request(err.to_string())),
        Err(err) => {
            error!(%err, "validation task panicked");
            finish(route, "panicked", start, StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

fn finish(route: &str, result: &str, start: Instant, response: Response) -> Response {
    metrics::sim_request(route, result, start);
    response
}

fn bad_request(message: String) -> Response {
    (StatusCode::BAD_REQUEST, message).into_response()
}

/// Replaces the disallow list, returning its new digest when it changed.
pub fn refresh_disallow(
    disallow: &DashSet<Address>,
    payload: DisallowListPayload,
) -> Option<String> {
    let previous = changed_disallow_hash(disallow, None);
    let parsed = payload.into_addresses();
    disallow.clear();
    for address in parsed {
        disallow.insert(address);
    }
    changed_disallow_hash(disallow, previous.as_deref())
}

const REFRESH_INTERVAL: time::Duration = time::Duration::from_secs(300);

pub async fn refresh_blacklist(endpoint: String, disallow: Arc<DashSet<Address>>) {
    let client = reqwest::Client::new();
    let mut interval = time::interval(REFRESH_INTERVAL);
    let mut loaded = false;
    loop {
        interval.tick().await;
        let failure = match client.get(&endpoint).send().await {
            Ok(response) if response.status().is_success() => {
                match response.json::<DisallowListPayload>().await {
                    Ok(payload) => {
                        if let Some(hash) = refresh_disallow(&disallow, payload) {
                            info!(%hash, size = disallow.len(), "disallow list updated");
                        }
                        loaded = true;
                        None
                    }
                    Err(err) => Some(format!("could not read the disallow list: {err}")),
                }
            }
            Ok(response) => Some(format!("disallow list fetch failed: HTTP {}", response.status())),
            Err(err) => Some(format!("disallow list fetch failed: {err}")),
        };

        if let Some(reason) = failure {
            if loaded {
                error!(%endpoint, "{reason}");
            } else {
                error!(
                    %endpoint,
                    "{reason}; no disallow list is in force, this builder applies no address filtering"
                );
            }
        }
    }
}
