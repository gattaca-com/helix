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
    simulator::SszValidationRequest,
};
use helix_types::Submission;
use ssz::Decode;
use tokio::net::TcpListener;
use tracing::error;

use crate::validation::{
    BlockSubmissionValidationApiServer, ExtendedValidationRequestV5, ValidationApi,
};

pub async fn run(api: ValidationApi, port: u16) {
    let router = Router::new().route("/validate", post(handler)).with_state(api);
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

async fn handler(
    State(api): State<ValidationApi>,
    body: axum::body::Bytes,
) -> Result<Response, DecoderError> {
    let req = SszValidationRequest::from_ssz_bytes(&body)?;

    let signed_bid_submission = match req.decoder_params {
        Some(decode_params) => {
            let mut buf = vec![];
            let mut decoder = SubmissionDecoder::new(&decode_params);
            let (submission, _, _) =
                decoder.decode(req.signed_bid_submission.as_slice(), &mut buf)?;
            match submission {
                Submission::Full(s) => s.into(),
                Submission::Dehydrated(_) => {
                    // Simulator-side hydration cache not yet implemented.
                    // Return 424 so the relay retries with full SSZ bytes.
                    return Ok(StatusCode::FAILED_DEPENDENCY.into_response());
                }
            }
        }
        None => SignedBidSubmissionV5::from_ssz_bytes(req.signed_bid_submission.as_slice())?,
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
