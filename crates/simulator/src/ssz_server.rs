use alloy_rpc_types::beacon::relay::{BuilderBlockValidationRequestV5, SignedBidSubmissionV5};
use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use helix_common::simulator::{SimRequest, SubmissionFormat};
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

async fn handler(State(api): State<ValidationApi>, body: axum::body::Bytes) -> impl IntoResponse {
    let req = match SimRequest::from_ssz_bytes(&body) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("ssz decode: {e:?}")).into_response(),
    };

    let signed_bid_submission = match req.format {
        SubmissionFormat::FullSsz => {
            match SignedBidSubmissionV5::from_ssz_bytes(&req.signed_bid_submission) {
                Ok(s) => s,
                Err(e) => {
                    return (StatusCode::BAD_REQUEST, format!("signed bid submission decode: {e:?}"))
                        .into_response()
                }
            }
        }
        SubmissionFormat::DehydratedSsz => {
            // Simulator-side hydration cache not yet implemented.
            // Return 424 so the relay retries with full SSZ bytes.
            return StatusCode::FAILED_DEPENDENCY.into_response();
        }
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

    match api.validate_builder_submission_v5(ext).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.message().to_string()).into_response(),
    }
}
