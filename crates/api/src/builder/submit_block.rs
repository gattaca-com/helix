use std::sync::Arc;

use axum::{http::StatusCode, Extension};
use helix_common::{
    self,
    bid_sorter::BidSorterMessage,
    bid_submission::{BidSubmission, OptimisticVersion},
    metadata_provider::MetadataProvider,
    metrics::ApiMetrics,
    task,
    utils::{extract_request_id, utcnow_ns},
    RequestTimings, SubmissionTrace,
};
use helix_database::DatabaseService;
use helix_types::BlockMergingPreferences;
use http::request::Parts;
use tracing::{debug, error, info, trace, warn, Instrument};

use super::api::BuilderApi;
use crate::{
    builder::{
        api::{decode_payload, get_mergeable_orders, sanity_check_block_submission},
        error::BuilderApiError,
    },
    proposer::MergingPoolMessage,
    Api,
};

impl<A: Api> BuilderApi<A> {
    /// Handles the submission of a new block by performing various checks and verifications
    /// before saving the submission to the auctioneer.
    ///
    /// 1. Receives the request and decodes the payload into a `SignedBidSubmission` object.
    /// 2. Validates the builder and checks against the next proposer duty.
    /// 3. Verifies the signature of the payload.
    /// 4. Runs further validations against auctioneer.
    /// 5. Simulates the block to validate the payment.
    /// 6. Saves the bid to auctioneer and db.
    ///
    /// Implements this API: <https://flashbots.github.io/relay-specs/#/Builder/submitBlock>
    #[tracing::instrument(skip_all, fields(
        id =% extract_request_id(&parts.headers),
        slot = tracing::field::Empty, // submission slot
        builder_pubkey = tracing::field::Empty,
        builder_id = tracing::field::Empty,
        block_hash = tracing::field::Empty,
    ), err)]
    pub async fn submit_block(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        Extension(timings): Extension<RequestTimings>,
        parts: Parts,
        body: bytes::Bytes,
    ) -> Result<StatusCode, BuilderApiError> {
        trace!("new block submission");

        let mut trace = SubmissionTrace::init_from_timings(timings);
        let (head_slot, next_duty) = api.curr_slot_info.slot_info();
        tracing::Span::current().record("slot", (head_slot.as_u64() + 1) as i64);

        // Verify that we have a validator connected for this slot
        let Some(next_duty) = next_duty else {
            return Err(BuilderApiError::ProposerDutyNotFound);
        };

        trace.metadata = api.metadata_provider.get_metadata(&parts.headers);

        // Decode the incoming request body into a payload
        let (skip_sigverify, payload_with_merging_data, is_cancellations_enabled) = decode_payload(
            head_slot.as_u64() + 1,
            &api,
            &parts.uri,
            &parts.headers,
            body,
            &mut trace,
        )
        .await?;
        let payload = payload_with_merging_data.submission;
        let merging_data = payload_with_merging_data.merging_data;
        ApiMetrics::cancellable_bid(is_cancellations_enabled);

        let block_hash = payload.message().block_hash;

        tracing::Span::current()
            .record("builder_pubkey", tracing::field::display(payload.builder_public_key()));
        tracing::Span::current()
            .record("block_hash", tracing::field::display(payload.message().block_hash));

        info!(
            decode_time = trace.decode.saturating_sub(trace.read_body),
            block_value = %payload.value(),
            "payload decoded",
        );

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

        // Fetch the next payload attributes and validate basic information
        let payload_attributes =
            api.fetch_payload_attributes(payload.slot(), *payload.parent_hash(), &block_hash)?;
        trace!("fetched payload attributes");

        // Handle duplicates.
        if let Err(err) = api.check_for_duplicate_block_hash(&block_hash) {
            match err {
                BuilderApiError::DuplicateBlockHash { block_hash } => {
                    // We dont return the error here as we want to continue processing the request.
                    // This mitigates the risk of someone sending an invalid payload
                    // with a valid header, which would block subsequent submissions with the same
                    // header and valid payload.
                    debug!(
                        ?block_hash,
                        builder_pub_key = ?payload.builder_public_key(),
                        "block hash already seen"
                    );
                }
                _ => return Err(err),
            }
        }
        trace!("checked for duplicates");

        // Verify the payload value is above the floor bid
        let floor_bid_value = api.get_current_floor(payload.slot());
        if payload.value() <= floor_bid_value {
            return Err(BuilderApiError::BidBelowFloor);
        }
        trace!(%floor_bid_value, "floor bid checked");
        trace.floor_bid_checks = utcnow_ns();

        // Fetch builder info
        let builder_info = api.fetch_builder_info(payload.builder_public_key());
        trace!(?builder_info, "fetched builder info");
        tracing::Span::current()
            .record("builder_id", tracing::field::display(builder_info.builder_id()));

        // Handle trusted builders check
        if !Self::check_if_trusted_builder(&next_duty, &builder_info) {
            let proposer_trusted_builders = next_duty.entry.preferences.trusted_builders.unwrap();
            debug!(
                builder_pub_key = ?payload.builder_public_key(),
                ?proposer_trusted_builders,
                "builder not in proposer trusted builders list",
            );
            return Err(BuilderApiError::BuilderNotInProposersTrustedList {
                proposer_trusted_builders,
            });
        }
        trace!("checked trusted builders");

        // Verify payload has not already been delivered
        if let Some(slot) = api.auctioneer.get_last_slot_delivered() {
            if payload.slot() <= slot {
                debug!("payload already delivered");
                return Err(BuilderApiError::PayloadAlreadyDelivered);
            }
        }
        trace!("checked payload not already delivered");

        // Sanity check the payload
        if let Err(err) = sanity_check_block_submission(
            &payload,
            &next_duty,
            &payload_attributes,
            &api.chain_info,
        ) {
            warn!(%err, "failed sanity check");
            return Err(err);
        }
        trace!("sanity check passed");
        trace.pre_checks = utcnow_ns();

        api.verify_signature(&payload, skip_sigverify, &mut trace)?;

        api.check_and_update_sequence_number(
            payload.builder_public_key(),
            head_slot + 1,
            &parts.headers,
        )?;

        let was_simulated_optimistically = api
            .simulate_submission(
                &payload,
                &builder_info,
                &mut trace,
                next_duty.entry,
                &payload_attributes,
            )
            .await?;

        let optimistic_version = if was_simulated_optimistically {
            OptimisticVersion::V1
        } else {
            OptimisticVersion::NotOptimistic
        };
        trace!(is_optimistic = was_simulated_optimistically, "verified submitted block");

        let merging_preferences =
            BlockMergingPreferences { allow_appending: merging_data.allow_appending };

        if let Err(err) = api.sorter_tx.try_send(BidSorterMessage::new_from_block_submission(
            &payload,
            &trace,
            optimistic_version,
            is_cancellations_enabled,
            merging_preferences,
            utcnow_ns(),
        )) {
            error!(?err, "failed to send submission to sorter");
            return Err(BuilderApiError::InternalError);
        };
        trace!("sent bid to bid sorter");

        // Skip mergeable orders extraction if block merging is not enabled
        if api.relay_config.block_merging_config.is_enabled {
            // In case the merging data is malformed, we log any error and discard it
            let mergeable_orders = get_mergeable_orders(&payload, merging_data)
                .inspect_err(|e| warn!(%e, "failed to get mergeable orders"))
                .ok();

            if mergeable_orders.as_ref().is_some_and(|o| !o.orders.is_empty()) {
                let orders = mergeable_orders.unwrap();
                let message = MergingPoolMessage::new(&payload, orders);
                // We only log the error if this fails
                let _ = api.merge_pool_tx.try_send(message).inspect_err(|err| {
                    error!(?err, "failed to send mergeable orders to merging pool");
                });
            }
        }

        // Save the execution payload
        api.auctioneer.save_execution_payload(
            payload.slot().as_u64(),
            payload.proposer_public_key(),
            payload.block_hash(),
            payload.payload_and_blobs_ref().to_owned(),
        );
        trace.auctioneer_update = utcnow_ns();
        trace!("saved payload to auctioneer");

        // Log some final info
        trace.request_finish = utcnow_ns();
        debug!(
            ?trace,
            request_duration_ns = trace.request_finish.saturating_sub(trace.receive),
            "submit_block request finished"
        );

        // Save submission to db.
        task::spawn(
            file!(),
            line!(),
            async move {
                if let Err(err) =
                    api.db.store_block_submission(payload, trace, optimistic_version).await
                {
                    error!(%err, "failed to store block submission")
                }
            }
            .in_current_span(),
        );

        Ok(StatusCode::OK)
    }
}

#[cfg(test)]
mod tests {
    use helix_common::chain_info::ChainInfo;
    use helix_types::{BidTrace, SignedBidSubmission, SignedBidSubmissionElectra, TestRandomSeed};
    use ssz::Encode;

    #[tokio::test]
    async fn test_locl() {
        let clock = ChainInfo::for_mainnet();

        let curr_slot = clock.current_slot();

        let bid_trace = BidTrace { slot: curr_slot.as_u64() + 1, ..BidTrace::test_random() };
        let sub = SignedBidSubmissionElectra {
            message: bid_trace,
            ..SignedBidSubmissionElectra::test_random()
        };

        let block = SignedBidSubmission::Electra(sub);
        let bytes = block.as_ssz_bytes();

        let res = reqwest::Client::builder()
            .build()
            .unwrap()
            .post("http://0.0.0.0:4040/relay/v1/builder/blocks")
            .header("Content-Type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .unwrap();

        println!("{}", res.status().as_str())
    }
}
