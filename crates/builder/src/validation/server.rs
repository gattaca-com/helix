use std::{net::SocketAddr, sync::Arc};

use alloy_primitives::Address;
use alloy_rpc_types::beacon::relay::SignedBidSubmissionV5;
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use dashmap::DashSet;
use helix_common::{
    blacklist::{changed_disallow_hash, parse_disallow_list},
    decoder::{DecoderError, SubmissionDecoder, SubmissionDecoderParams},
    simulator::{SszMergedValidationRequest, SszValidationRequest},
};
use helix_types::Submission;
use ssz::Decode;
use tokio::{net::TcpListener, sync::Semaphore, time};
use tracing::{error, info, warn};

use crate::{
    engine::convert::eblobs,
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
    let request = match SszValidationRequest::from_ssz_bytes(&body) {
        Ok(request) => request,
        Err(err) => return bad_request(format!("{err:?}")),
    };
    let submission = match decode_submission(request.decoder_params, &request.signed_bid_submission)
    {
        Ok(Some(submission)) => submission,
        Ok(None) => return StatusCode::FAILED_DEPENDENCY.into_response(),
        Err(err) => return bad_request(err.to_string()),
    };

    run_validation(state, move |validator| {
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
    let request = match SszMergedValidationRequest::from_ssz_bytes(&body) {
        Ok(request) => request,
        Err(err) => return bad_request(format!("{err:?}")),
    };
    let submission = match decode_submission(request.decoder_params, &request.signed_bid_submission)
    {
        Ok(Some(submission)) => submission,
        Ok(None) => return StatusCode::FAILED_DEPENDENCY.into_response(),
        Err(err) => return bad_request(err.to_string()),
    };

    run_validation(state, move |validator| {
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
/// The semaphore caps how many run at once.
async fn run_validation<F>(state: ServerState, validate: F) -> Response
where
    F: FnOnce(&BlockValidator) -> Result<crate::validation::ExecutedBlock, ValidationError>
        + Send
        + 'static,
{
    let Ok(_permit) = state.permits.clone().acquire_owned().await else {
        return bad_request("validation server is shutting down".to_string());
    };
    let validator = state.validator.clone();
    let result = tokio::task::spawn_blocking(move || validate(&validator).map(|_| ())).await;

    match result {
        Ok(Ok(())) => StatusCode::OK.into_response(),
        Ok(Err(err)) => bad_request(err.to_string()),
        Err(err) => {
            error!(%err, "validation task panicked");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn bad_request(message: String) -> Response {
    (StatusCode::BAD_REQUEST, message).into_response()
}

/// Replaces the disallow list, returning its new digest when it changed.
pub fn refresh_disallow(disallow: &DashSet<Address>, list: Vec<String>) -> Option<String> {
    let previous = changed_disallow_hash(disallow, None);
    let parsed = parse_disallow_list(list);
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
    loop {
        interval.tick().await;
        match client.get(&endpoint).send().await {
            Ok(response) if response.status().is_success() => match response.json().await {
                Ok(list) => {
                    if let Some(hash) = refresh_disallow(&disallow, list) {
                        info!(%hash, size = disallow.len(), "disallow list updated");
                    }
                }
                Err(err) => warn!(%err, "could not read the disallow list"),
            },
            Ok(response) => warn!(status = %response.status(), "disallow list fetch failed"),
            Err(err) => warn!(%err, "disallow list fetch failed"),
        }
    }
}
