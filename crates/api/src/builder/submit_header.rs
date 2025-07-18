use std::sync::Arc;

use alloy_primitives::U256;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Extension,
};
use helix_common::{
    self,
    bid_submission::{
        v2::header_submission::SignedHeaderSubmission,
        v3::header_submission_v3::HeaderSubmissionV3, BidSubmission,
    },
    metadata_provider::MetadataProvider,
    metrics::ApiMetrics,
    task,
    utils::{extract_request_id, utcnow_ns},
    HeaderSubmissionTrace,
};
use helix_database::DatabaseService;
use helix_datastore::{types::SaveBidAndUpdateTopBidResponse, Auctioneer};
use helix_types::SignedBuilderBid;
use hyper::HeaderMap;
use ssz::Decode;
use tracing::{debug, error, info, warn, Instrument};

use super::api::{log_save_bid_info, BuilderApi};
use crate::{
    builder::{
        api::{sanity_check_block_submission, MAX_PAYLOAD_LENGTH},
        error::BuilderApiError,
    },
    proposer::{ShareHeader, HELIX_SHARE_HEADER},
    Api,
};

impl<A: Api> BuilderApi<A> {
    /// Handles the submission of a new payload header by performing various checks and
    /// verifications before saving the header to the auctioneer.
    #[tracing::instrument(skip_all, fields(id =% extract_request_id(&headers)))]
    pub async fn submit_header(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        headers: HeaderMap,
        req: Request<Body>,
    ) -> Result<StatusCode, BuilderApiError> {
        let mut trace = HeaderSubmissionTrace { receive: utcnow_ns(), ..Default::default() };
        trace.metadata = api.metadata_provider.get_metadata(&headers);

        debug!(timestamp_request_start = trace.receive,);

        let sharing = headers.get(HELIX_SHARE_HEADER).map(ShareHeader::from).unwrap_or_default();

        // Decode the incoming request body into a payload
        let (payload, is_cancellations_enabled) = decode_header_submission(req, &mut trace).await?;
        ApiMetrics::cancellable_bid(is_cancellations_enabled);

        Self::handle_submit_header(
            &api,
            payload,
            None,
            None,
            is_cancellations_enabled,
            api.relay_config.header_gossip_enabled && matches!(sharing, ShareHeader::All),
            trace,
        )
        .await
    }

    #[tracing::instrument(skip_all, fields(id =% extract_request_id(&headers)))]
    pub async fn submit_header_v3(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        headers: HeaderMap,
        req: Request<Body>,
    ) -> Result<StatusCode, BuilderApiError> {
        let mut trace = HeaderSubmissionTrace { receive: utcnow_ns(), ..Default::default() };
        trace.metadata = api.metadata_provider.get_metadata(&headers);

        debug!(timestamp_request_start = trace.receive,);

        // Decode the incoming request body into a payload
        let (payload, is_cancellations_enabled) =
            decode_header_submission_v3(req, &mut trace).await?;

        let sharing = headers.get(HELIX_SHARE_HEADER).map(ShareHeader::from).unwrap_or_default();

        Self::handle_submit_header(
            &api,
            payload.submission,
            Some(payload.url),
            Some(payload.tx_count),
            is_cancellations_enabled,
            matches!(sharing, ShareHeader::All),
            trace,
        )
        .await
    }

    pub(crate) async fn handle_submit_header(
        api: &Arc<BuilderApi<A>>,
        payload: SignedHeaderSubmission,
        payload_address: Option<Vec<u8>>,
        block_tx_count: Option<u32>,
        is_cancellations_enabled: bool,
        is_gossip_enabled: bool,
        mut trace: HeaderSubmissionTrace,
    ) -> Result<StatusCode, BuilderApiError> {
        let (head_slot, next_duty) = api.curr_slot_info.slot_info();

        let block_hash = payload.block_hash();

        // Verify the payload is for the current slot
        if payload.slot() < head_slot + 1 {
            return Err(BuilderApiError::SubmissionForPastSlot {
                expected: head_slot + 1,
                got: payload.slot(),
            });
        }

        if payload.slot() > head_slot + 1 {
            return Err(BuilderApiError::SubmissionForFutureSlot {
                expected: head_slot + 1,
                got: payload.slot(),
            });
        }

        info!(
            slot = %payload.slot(),
            builder_pub_key = ?payload.builder_public_key(),
            block_value = %payload.value(),
            ?block_hash,
            "header submission decoded",
        );

        // Verify that we have a validator connected for this slot
        let Some(next_duty) = next_duty else {
            warn!(?block_hash, "could not find slot duty");
            return Err(BuilderApiError::ProposerDutyNotFound);
        };

        // Fetch the next payload attributes and validate basic information
        let payload_attributes =
            api.fetch_payload_attributes(payload.slot(), *payload.parent_hash(), block_hash)?;

        // Fetch builder info
        let builder_info = api.fetch_builder_info(payload.builder_public_key()).await;

        // Submit header can only be processed optimistically.
        // Make sure that the builder has enough collateral to cover the submission.
        if let Err(err) = Self::check_builder_collateral(&payload, &builder_info) {
            warn!(%err, "builder has insufficient collateral");
            return Err(err);
        }

        // Handle duplicates.
        if let Err(err) = api.check_for_duplicate_block_hash(block_hash).await {
            match err {
                BuilderApiError::DuplicateBlockHash { block_hash } => {
                    // We dont return the error here as we want to continue processing the request.
                    // This mitigates the risk of someone sending an invalid payload
                    // with a valid header, which would block subsequent submissions with the same
                    // header and valid payload.
                    debug!(?block_hash, builder_pub_key = ?payload.builder_public_key(), "block hash already seen");
                }
                _ => return Err(err),
            }
        }

        // Discard any OptimisticV2 submissions if the proposer has regional filtering enabled
        // and the builder is not optimistic for regional filtering.
        if next_duty.entry.preferences.filtering.is_regional() &&
            !builder_info.can_process_regional_slot_optimistically()
        {
            warn!("proposer has regional filtering and builder is not optimistic for regional filtering, discarding optimistic v2 submission");
            return Err(BuilderApiError::BuilderNotOptimistic {
                builder_pub_key: payload.builder_public_key().clone(),
            });
        }

        // Validate basic information about the payload
        if let Err(err) = sanity_check_block_submission(
            &payload,
            &next_duty,
            &payload_attributes,
            &api.chain_info,
        ) {
            warn!(%err, "failed sanity check");
            return Err(err);
        }

        // Handle trusted builders check
        if !Self::check_if_trusted_builder(&next_duty, &builder_info) {
            let proposer_trusted_builders = next_duty.entry.preferences.trusted_builders.unwrap();
            debug!(
                builder_pub_key = ?payload.builder_public_key(),
                proposer_trusted_builders = ?proposer_trusted_builders,
                "builder not in proposer trusted builders list",
            );
            return Err(BuilderApiError::BuilderNotInProposersTrustedList {
                proposer_trusted_builders,
            });
        }

        trace.pre_checks = utcnow_ns();

        // Verify the payload signature
        if let Err(err) = payload.verify_signature(&api.chain_info.context) {
            warn!(%err, "failed to verify signature");
            return Err(BuilderApiError::SignatureVerificationFailed);
        }
        trace.signature = utcnow_ns();

        // Verify payload has not already been delivered
        match api.auctioneer.get_last_slot_delivered().await {
            Ok(Some(slot)) => {
                if payload.slot() <= slot {
                    debug!("payload already delivered");
                    return Err(BuilderApiError::PayloadAlreadyDelivered);
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!(%err, "failed to get last slot delivered");
            }
        }

        let payload = Arc::new(payload);

        // Verify the payload value is above the floor bid
        let floor_bid_value = api
            .check_if_bid_is_below_floor(
                payload.slot().as_u64(),
                payload.parent_hash(),
                payload.proposer_public_key(),
                payload.builder_public_key(),
                payload.value(),
                is_cancellations_enabled,
            )
            .await?;
        trace.floor_bid_checks = utcnow_ns();

        // Save bid to auctioneer
        match api
            .save_header_bid_to_auctioneer(
                payload.clone(),
                &mut trace,
                is_cancellations_enabled,
                floor_bid_value,
            )
            .await?
        {
            // v3 header submissions (`block_tx_count` is set) are not gossiped
            Some(builder_bid) if block_tx_count.is_none() => {
                if is_gossip_enabled {
                    api.gossip_header(
                        builder_bid,
                        payload.bid_trace().clone(),
                        is_cancellations_enabled,
                        trace.receive,
                        payload_address.clone(),
                    )
                    .await;
                }
            }
            _ => { /* Bid wasn't saved so no need to gossip as it will never be served */ }
        }

        // Log some final info
        trace.request_finish = utcnow_ns();
        info!(
            ?trace,
            request_duration_ns = trace.request_finish.saturating_sub(trace.receive),
            "submit_header request finished"
        );

        match payload_address {
            None => {
                // Save pending block header to auctioneer
                api.auctioneer
                    .save_pending_block_header(
                        payload.slot().as_u64(),
                        payload.builder_public_key(),
                        payload.block_hash(),
                        trace.receive / 1_000_000, // convert to ms
                    )
                    .await
                    .map_err(|err| {
                        error!(%err, "failed to save pending block header");
                        BuilderApiError::AuctioneerError(err)
                    })?;
            }
            Some(payload_addr) => {
                api.auctioneer
                    .save_payload_address(
                        payload.block_hash(),
                        payload.builder_public_key(),
                        payload_addr,
                    )
                    .await
                    .map_err(|err| {
                        error!(%err, "failed to save payload address");
                        BuilderApiError::AuctioneerError(err)
                    })?;
            }
        }

        // Save submission to db
        let db = api.db.clone();
        task::spawn(
            file!(),
            line!(),
            async move {
                if let Err(err) = db.store_header_submission(payload, trace, block_tx_count).await {
                    error!(
                        %err,
                        "failed to store header submission",
                    )
                }
            }
            .in_current_span(),
        );

        Ok(StatusCode::OK)
    }

    async fn save_header_bid_to_auctioneer(
        &self,
        payload: Arc<SignedHeaderSubmission>,
        trace: &mut HeaderSubmissionTrace,
        is_cancellations_enabled: bool,
        floor_bid_value: U256,
    ) -> Result<Option<SignedBuilderBid>, BuilderApiError> {
        let mut update_bid_result = SaveBidAndUpdateTopBidResponse::default();
        match self
            .auctioneer
            .save_header_submission_and_update_top_bid(
                &payload,
                trace.receive.into(),
                is_cancellations_enabled,
                floor_bid_value,
                &mut update_bid_result,
                &self.signing_context,
            )
            .await
        {
            Ok(Some(builder_bid)) => {
                // Log the results of the bid submission
                trace.auctioneer_update = utcnow_ns();
                log_save_bid_info(
                    &update_bid_result,
                    trace.floor_bid_checks,
                    trace.auctioneer_update,
                );

                Ok(Some(builder_bid))
            }
            Ok(None) => Ok(None),
            Err(err) => {
                error!(%err, "could not save header submission and update top bid");
                Err(BuilderApiError::AuctioneerError(err))
            }
        }
    }
}

/// `decode_payload` decodes the payload from `SubmitBlockParams` into a `SignedHeaderSubmission`
/// object.
///
/// - Supports both SSZ and JSON encodings for deserialization.
/// - Automatically falls back to JSON if SSZ deserialization fails.
/// - Does *not* handle GZIP-compressed headers.
///
/// It returns a tuple of the decoded header and if cancellations are enabled.
pub async fn decode_header_submission(
    req: Request<Body>,
    trace: &mut HeaderSubmissionTrace,
) -> Result<(SignedHeaderSubmission, bool), BuilderApiError> {
    // Extract the query parameters
    let is_cancellations_enabled = req
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .find_map(|part| {
            let mut split = part.splitn(2, '=');
            if split.next()? == "cancellations" {
                Some(split.next()? == "1")
            } else {
                None
            }
        })
        .unwrap_or(false);

    info!(
        headers = ?req.headers(),
        "received header",
    );

    let is_ssz = req.headers().get("Content-Type").and_then(|val| val.to_str().ok()) ==
        Some("application/octet-stream");

    // Read the body
    let body = req.into_body();
    let body_bytes = to_bytes(body, MAX_PAYLOAD_LENGTH).await?;
    if body_bytes.len() > MAX_PAYLOAD_LENGTH {
        return Err(BuilderApiError::PayloadTooLarge {
            max_size: MAX_PAYLOAD_LENGTH,
            size: body_bytes.len(),
        });
    }

    // Decode header
    let header: SignedHeaderSubmission = if is_ssz {
        match SignedHeaderSubmission::from_ssz_bytes(&body_bytes) {
            Ok(header) => header,
            Err(err) => {
                // Fallback to JSON
                warn!(?err, "Failed to decode header using SSZ; falling back to JSON");
                serde_json::from_slice(&body_bytes)?
            }
        }
    } else {
        serde_json::from_slice(&body_bytes)?
    };

    trace.decode = utcnow_ns();
    debug!(
        timestamp_after_decoding = trace.decode,
        decode_latency_ns = trace.decode.saturating_sub(trace.receive),
        builder_pub_key = ?header.builder_public_key(),
        block_hash = ?header.block_hash(),
        proposer_pubkey = ?header.proposer_public_key(),
        parent_hash = ?header.parent_hash(),
        value = ?header.value(),
    );

    Ok((header, is_cancellations_enabled))
}

pub async fn decode_header_submission_v3(
    req: Request<Body>,
    trace: &mut HeaderSubmissionTrace,
) -> Result<(HeaderSubmissionV3, bool), BuilderApiError> {
    // Extract the query parameters
    let is_cancellations_enabled = req
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .find_map(|part| {
            let mut split = part.splitn(2, '=');
            if split.next()? == "cancellations" {
                Some(split.next()? == "1")
            } else {
                None
            }
        })
        .unwrap_or(false);

    info!(
        headers = ?req.headers(),
        "received header",
    );

    // Get content-type header
    let content_type = req.headers().get("Content-Type").cloned();

    // Read the body
    let body = req.into_body();
    let body_bytes = to_bytes(body, MAX_PAYLOAD_LENGTH).await?;
    if body_bytes.len() > MAX_PAYLOAD_LENGTH {
        return Err(BuilderApiError::PayloadTooLarge {
            max_size: MAX_PAYLOAD_LENGTH,
            size: body_bytes.len(),
        });
    }

    let submission_v3 = match content_type.as_ref().and_then(|val| val.to_str().ok()) {
        Some("application/octet-stream") => HeaderSubmissionV3::from_ssz_bytes(&body_bytes)
            .map_err(|_| BuilderApiError::DeserializeError)?,
        Some("application/cbor") => cbor4ii::serde::from_slice(&body_bytes)
            .map_err(|_| BuilderApiError::DeserializeError)?,
        _ => serde_json::from_slice(&body_bytes)?,
    };

    trace.decode = utcnow_ns();
    debug!(
        timestamp_after_decoding = trace.decode,
        decode_latency_ns = trace.decode.saturating_sub(trace.receive),
        builder_pub_key = ?submission_v3.submission.builder_public_key(),
        block_hash = ?submission_v3.submission.block_hash(),
        proposer_pubkey = ?submission_v3.submission.proposer_public_key(),
        parent_hash = ?submission_v3.submission.parent_hash(),
        value = ?submission_v3.submission.value(),
    );

    Ok((submission_v3, is_cancellations_enabled))
}
