use alloy_rpc_types::beacon::relay::{BuilderBlockValidationRequestV5, SignedBidSubmissionV5};
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use helix_common::{
    decoder::{DecoderError, SubmissionDecoder},
    simulator::{SszMergedValidationRequest, SszValidationRequest},
};
use helix_types::Submission;
use ssz::Decode;
use tokio::net::TcpListener;
use tracing::error;

use crate::validation::{
    BlockSubmissionValidationApiServer, ExtendedMergedValidationRequestV5,
    ExtendedValidationRequestV5, ValidationApi,
};

pub async fn run(api: ValidationApi, port: u16) {
    let router = Router::new()
        .route("/validate", post(handler))
        .route("/validate_merged", post(merged_handler))
        .with_state(api);
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            error!(%e, port, "failed to bind SSZ sim server");
            return;
        }
    };
    if let Err(e) = axum::serve(listener, router).await {
        error!(%e, "SSZ sim server exited");
    }
}

/// Decodes the submission carried by an SSZ validation request body, in either shape: full
/// bytes, or a dehydrated reference (which this server can't yet rehydrate -- see the 424
/// below).
fn decode_submission(
    decoder_params: Option<helix_common::decoder::SubmissionDecoderParams>,
    signed_bid_submission: &[u8],
) -> Result<Result<SignedBidSubmissionV5, Response>, DecoderError> {
    Ok(match decoder_params {
        Some(decode_params) => {
            let mut buf = vec![];
            let mut decoder = SubmissionDecoder::new(&decode_params);
            let (submission, _, _) = decoder.decode(signed_bid_submission, &mut buf)?;
            match submission {
                Submission::Full(s) => Ok(s.into()),
                Submission::Dehydrated(_) => {
                    // Simulator-side hydration cache not yet implemented.
                    // Return 424 so the relay retries with full SSZ bytes.
                    Err(StatusCode::FAILED_DEPENDENCY.into_response())
                }
            }
        }
        None => Ok(SignedBidSubmissionV5::from_ssz_bytes(signed_bid_submission)?),
    })
}

async fn handler(
    State(api): State<ValidationApi>,
    body: axum::body::Bytes,
) -> Result<Response, DecoderError> {
    let req = SszValidationRequest::from_ssz_bytes(&body)?;

    let signed_bid_submission =
        match decode_submission(req.decoder_params, &req.signed_bid_submission)? {
            Ok(submission) => submission,
            Err(early_response) => return Ok(early_response),
        };

    let ext = ExtendedValidationRequestV5 {
        base: BuilderBlockValidationRequestV5 {
            request: signed_bid_submission,
            registered_gas_limit: req.registered_gas_limit,
            parent_beacon_block_root: req.parent_beacon_block_root,
        },
        inclusion_list: Some(req.inclusion_list),
        apply_blacklist: req.apply_blacklist,
    };

    Ok(match api.validate_builder_submission_v5(ext).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.message().to_string()).into_response(),
    })
}

/// Relay-internal merged-block route; never reachable by an externally submitted builder
/// block. See `ExtendedMergedValidationRequestV5`.
async fn merged_handler(
    State(api): State<ValidationApi>,
    body: axum::body::Bytes,
) -> Result<Response, DecoderError> {
    let req = SszMergedValidationRequest::from_ssz_bytes(&body)?;

    let signed_bid_submission =
        match decode_submission(req.decoder_params, &req.signed_bid_submission)? {
            Ok(submission) => submission,
            Err(early_response) => return Ok(early_response),
        };

    let ext = ExtendedMergedValidationRequestV5 {
        base: ExtendedValidationRequestV5 {
            base: BuilderBlockValidationRequestV5 {
                request: signed_bid_submission,
                registered_gas_limit: req.registered_gas_limit,
                parent_beacon_block_root: req.parent_beacon_block_root,
            },
            inclusion_list: Some(req.inclusion_list),
            apply_blacklist: req.apply_blacklist,
        },
        base_payment_tx_index: req.base_payment_tx_index,
    };

    Ok(match api.validate_merged_builder_submission_v5(ext).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.message().to_string()).into_response(),
    })
}
