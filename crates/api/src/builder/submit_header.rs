use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Extension,
};
use helix_common::{
    self,
    bid_sorter::BidSorterMessage,
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
use helix_datastore::Auctioneer;
use hyper::HeaderMap;
use ssz::Decode;
use tracing::{debug, error, info, trace, warn, Instrument};

use super::api::BuilderApi;
use crate::{
    builder::{
        api::{sanity_check_block_submission, MAX_PAYLOAD_LENGTH},
        error::BuilderApiError,
        v2_check::V2SubMessage,
    },
    Api,
};

impl<A: Api> BuilderApi<A> {
    /// Handles the submission of a new payload header by performing various checks and
    /// verifications before saving the header to the auctioneer.
    #[tracing::instrument(skip_all, fields(id =% extract_request_id(&headers)))]
    pub async fn submit_header(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        Extension(on_receive_ns): Extension<u64>,
        headers: HeaderMap,
        req: Request<Body>,
    ) -> Result<StatusCode, BuilderApiError> {
        let mut trace = HeaderSubmissionTrace { receive: on_receive_ns, ..Default::default() };
        trace.metadata = api.metadata_provider.get_metadata(&headers);

        debug!(timestamp_request_start = trace.receive,);

        // Decode the incoming request body into a payload
        let (payload, is_cancellations_enabled) = decode_header_submission(req, &mut trace).await?;
        ApiMetrics::cancellable_bid(is_cancellations_enabled);

        Self::handle_submit_header(&api, payload, None, None, is_cancellations_enabled, trace).await
    }

    #[tracing::instrument(skip_all, fields(id =% extract_request_id(&headers)))]
    pub async fn submit_header_v3(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        Extension(on_receive_ns): Extension<u64>,
        headers: HeaderMap,
        req: Request<Body>,
    ) -> Result<StatusCode, BuilderApiError> {
        let mut trace = HeaderSubmissionTrace { receive: on_receive_ns, ..Default::default() };
        trace.metadata = api.metadata_provider.get_metadata(&headers);

        debug!(timestamp_request_start = trace.receive,);

        // Decode the incoming request body into a payload
        let (payload, is_cancellations_enabled) =
            decode_header_submission_v3(req, &mut trace).await?;

        Self::handle_submit_header(
            &api,
            payload.submission,
            Some(payload.url),
            Some(payload.tx_count),
            is_cancellations_enabled,
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
        let builder_info = api.fetch_builder_info(payload.builder_public_key());

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
        if let Some(slot) = api.auctioneer.get_last_slot_delivered() {
            if payload.slot() <= slot {
                debug!("payload already delivered");
                return Err(BuilderApiError::PayloadAlreadyDelivered);
            }
        }

        let payload = Arc::new(payload);

        // Verify the payload value is above the floor bid
        let floor_bid_value = api.get_current_floor(payload.slot());
        if payload.value() <= floor_bid_value {
            return Err(BuilderApiError::BidBelowFloor);
        }
        trace!(%floor_bid_value, "floor bid checked");

        trace.floor_bid_checks = utcnow_ns();

        if let Err(err) = api.sorter_tx.try_send(BidSorterMessage::new_from_header_submission(
            &payload,
            trace.receive,
            is_cancellations_enabled,
            utcnow_ns(),
        )) {
            error!(?err, "failed to send submission to sorter");
            return Err(BuilderApiError::InternalError);
        };

        // Log some final info
        trace.request_finish = utcnow_ns();
        info!(
            ?trace,
            request_duration_ns = trace.request_finish.saturating_sub(trace.receive),
            "submit_header request finished"
        );

        if payload_address.is_none() {
            if let Err(err) = api
                .v2_checks_tx
                .try_send(V2SubMessage::new_from_header_submission(&payload, trace.receive))
            {
                error!(%err, "failed to send block to v2 checker");
            }
        }

        if let Some(payload_addr) = payload_address {
            api.auctioneer.save_payload_address(
                payload.block_hash(),
                payload.builder_public_key(),
                payload_addr,
            );
        };

        api.tx_root_cache
            .insert(*payload.block_hash(), (payload.slot().as_u64(), payload.transactions_root()));

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
        SignedHeaderSubmission::from_ssz_bytes(&body_bytes)
            .map_err(|err| BuilderApiError::SszDeserializeError(format!("{err:?}")))?
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
            .map_err(|err| BuilderApiError::SszDeserializeError(format!("{err:?}")))?,
        Some("application/cbor") => cbor4ii::serde::from_slice(&body_bytes)?,
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
