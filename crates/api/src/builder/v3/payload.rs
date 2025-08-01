use std::sync::Arc;

use alloy_primitives::B256;
use helix_common::{
    bid_submission::{
        v3::header_submission_v3::{GetPayloadV3, SignedGetPayloadV3},
        OptimisticVersion,
    },
    signing::RelaySigningContext,
    utils::{utcnow_ms, utcnow_ns},
    SubmissionTrace,
};
use helix_types::{BlsPublicKey, SignedBidSubmission};
use reqwest::Url;
use ssz::{Decode, Encode};
use tokio::sync::mpsc::Receiver;
use tracing::{error, info, warn};

use super::V3Error;
use crate::{
    builder::{api::BuilderApi, error::BuilderApiError},
    Api,
};

/// A task that fetches builder blocks for optimistic v3 submissions.
pub async fn fetch_builder_blocks<A: Api>(
    api: Arc<BuilderApi<A>>,
    mut receiver: Receiver<(u64, B256, BlsPublicKey, Vec<u8>)>,
    signing_ctx: Arc<RelaySigningContext>,
) {
    while let Some((slot, block_hash, builder_pubkey, builder_address)) = receiver.recv().await {
        let receive = utcnow_ns();
        let trace = SubmissionTrace { receive, ..Default::default() };

        info!(?block_hash, "fetch v3 builder payload");
        match fetch_block(block_hash, &builder_address, &signing_ctx).await {
            Ok(block) => match BuilderApi::handle_optimistic_payload(
                api.clone(),
                block,
                trace,
                OptimisticVersion::V3,
            )
            .await
            {
                Ok(_) => info!(?block_hash, ?builder_address, "v3 block fetch successful"),
                Err(err) => {
                    error!(%err, ?block_hash, ?builder_address, "v3 block submission failed")
                }
            },
            Err(e) => {
                error!(error=?e, ?block_hash, ?builder_address, "v3 block fetch failed, demoting builder");
                api.demote_builder(
                    slot,
                    &builder_pubkey,
                    &block_hash,
                    &BuilderApiError::PayloadError(e),
                )
                .await;
            }
        }
    }

    warn!("v3 payload fetch task exiting");
}

async fn fetch_block(
    block_hash: B256,
    payload_address: &[u8],
    signing_ctx: &RelaySigningContext,
) -> Result<SignedBidSubmission, V3Error> {
    let message = GetPayloadV3 {
        block_hash,
        request_ts_millis: utcnow_ms(),
        relay_pubkey: signing_ctx.pubkey().clone(),
    };
    let signature = signing_ctx.sign_builder_message(&message);
    let signed_request = SignedGetPayloadV3 { message, signature };
    let request_bytes = signed_request.as_ssz_bytes();

    // Make the payload request.
    let builder_url = String::from_utf8(payload_address.to_owned())
        .map_err(|e| V3Error::BuilderAddressError(e.to_string()))?;
    let payload_url = Url::parse(&format!("{builder_url}/get_payload_v3"))
        .map_err(|e| V3Error::BuilderAddressError(e.to_string()))?;

    let client = reqwest::Client::new();
    let response = client
        .post(payload_url)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(request_bytes)
        .send()
        .await?;
    let response_bytes = response.bytes().await?;

    SignedBidSubmission::from_ssz_bytes(&response_bytes).map_err(V3Error::Ssz)
}
