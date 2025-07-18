use std::sync::Arc;

use alloy_primitives::U256;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Extension,
};
use helix_common::{
    self,
    bid_submission::BidSubmission,
    metadata_provider::MetadataProvider,
    task,
    utils::{extract_request_id, utcnow_ns},
    SubmissionTrace,
};
use helix_database::DatabaseService;
use helix_datastore::{types::SaveBidAndUpdateTopBidResponse, Auctioneer};
use helix_types::SignedBidSubmission;
use hyper::HeaderMap;
use tracing::{debug, error, info, trace, warn, Instrument, Level};

use super::api::{log_save_bid_info, BuilderApi};
use crate::{
    builder::{
        api::{decode_payload, sanity_check_block_submission},
        error::BuilderApiError,
        OptimisticVersion,
    },
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
        id =% extract_request_id(&headers),
        slot = tracing::field::Empty, // submission slot
        builder_pubkey = tracing::field::Empty,
        builder_id = tracing::field::Empty,
        block_hash = tracing::field::Empty,
    ), err, ret(level = Level::DEBUG))]
    pub async fn submit_block(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        headers: HeaderMap,
        req: Request<Body>,
    ) -> Result<StatusCode, BuilderApiError> {
        trace!("new block submission");

        let mut trace = SubmissionTrace { receive: utcnow_ns(), ..Default::default() };
        let (head_slot, next_duty) = api.curr_slot_info.slot_info();
        tracing::Span::current().record("slot", (head_slot.as_u64() + 1) as i64);

        // Verify that we have a validator connected for this slot
        let Some(next_duty) = next_duty else {
            warn!("could not find slot duty");
            return Err(BuilderApiError::ProposerDutyNotFound);
        };

        trace.metadata = api.metadata_provider.get_metadata(&headers);

        debug!(%head_slot, timestamp_request_start = trace.receive);

        // Decode the incoming request body into a payload
        let (payload, is_cancellations_enabled) = decode_payload(req, &mut trace).await?;
        let block_hash = payload.message().block_hash;

        tracing::Span::current()
            .record("builder_pubkey", tracing::field::display(payload.builder_public_key()));
        tracing::Span::current()
            .record("block_hash", tracing::field::display(payload.message().block_hash));

        info!(
            slot = %payload.slot(),
            block_hash = %block_hash,
            builder_pub_key = %payload.builder_public_key(),
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
        if let Err(err) = api.check_for_duplicate_block_hash(&block_hash).await {
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
        let floor_bid_value = api
            .check_if_bid_is_below_floor(
                payload.slot().into(),
                payload.parent_hash(),
                payload.proposer_public_key(),
                payload.builder_public_key(),
                payload.value(),
                is_cancellations_enabled,
            )
            .await?;
        trace!(%floor_bid_value, "floor bid checked");
        trace.floor_bid_checks = utcnow_ns();

        // Fetch builder info
        let builder_info = api.fetch_builder_info(payload.builder_public_key()).await;
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

        let was_simulated_optimistically = api
            .verify_submitted_block(
                payload.clone(),
                next_duty,
                &builder_info,
                &mut trace,
                &payload_attributes,
            )
            .await?;
        trace!(is_optimistic = was_simulated_optimistically, "verified submitted block");

        // If cancellations are enabled, then abort now if there is a later submission
        if is_cancellations_enabled {
            if let Err(err) = api.check_for_later_submissions(&payload, trace.receive).await {
                warn!(%err, "already processing later submission");

                // Save bid to auctioneer
                if let Err(err) = api
                    .auctioneer
                    .save_execution_payload(
                        payload.slot().into(),
                        &payload.message().proposer_pubkey,
                        payload.block_hash(),
                        &payload.payload_and_blobs(),
                    )
                    .await
                {
                    error!(%err, "failed to save execution payload");
                    return Err(BuilderApiError::AuctioneerError(err));
                }

                // Log some final info
                trace.request_finish = utcnow_ns();
                debug!(
                    ?trace,
                    request_duration_ns = trace.request_finish.saturating_sub(trace.receive),
                    "submit_block request finished"
                );

                let optimistic_version = if was_simulated_optimistically {
                    OptimisticVersion::V1
                } else {
                    OptimisticVersion::NotOptimistic
                };

                // Save submission to db.
                task::spawn(
                    file!(),
                    line!(),
                    async move {
                        if let Err(err) = api
                            .db
                            .store_block_submission(payload, trace, optimistic_version as i16)
                            .await
                        {
                            error!(%err, "failed to store block submission")
                        }
                    }
                    .in_current_span(),
                );

                return Err(err);
            }
        }
        trace!(is_cancellations_enabled, "checked for later submissions");

        // Save bid to auctioneer
        api.save_bid_to_auctioneer(&payload, &mut trace, is_cancellations_enabled, floor_bid_value)
            .await?;
        trace!("saved bid to auctioneer");

        // Log some final info
        trace.request_finish = utcnow_ns();
        debug!(
            ?trace,
            request_duration_ns = trace.request_finish.saturating_sub(trace.receive),
            "submit_block request finished"
        );

        let optimistic_version = if was_simulated_optimistically {
            OptimisticVersion::V1
        } else {
            OptimisticVersion::NotOptimistic
        };

        // Save submission to db.
        task::spawn(
            file!(),
            line!(),
            async move {
                if let Err(err) =
                    api.db.store_block_submission(payload, trace, optimistic_version as i16).await
                {
                    error!(%err, "failed to store block submission")
                }
            }
            .in_current_span(),
        );

        Ok(StatusCode::OK)
    }

    async fn save_bid_to_auctioneer(
        &self,
        payload: &SignedBidSubmission,
        trace: &mut SubmissionTrace,
        is_cancellations_enabled: bool,
        floor_bid_value: U256,
    ) -> Result<(), BuilderApiError> {
        let mut update_bid_result = SaveBidAndUpdateTopBidResponse::default();

        match self
            .auctioneer
            .save_bid_and_update_top_bid(
                payload,
                trace.receive.into(),
                is_cancellations_enabled,
                floor_bid_value,
                &mut update_bid_result,
                &self.signing_context,
            )
            .await
        {
            Ok(_) => {
                // Log the results of the bid submission
                trace.auctioneer_update = utcnow_ns();
                log_save_bid_info(&update_bid_result, trace.simulation, trace.auctioneer_update);

                Ok(())
            }

            Err(err) => {
                error!(%err, "could not save bid and update top bids");
                Err(BuilderApiError::AuctioneerError(err))
            }
        }
    }

    /// Checks for later bid submissions from the same builder.
    ///
    /// This function should be called only if cancellations are enabled.
    /// It checks if there is a later submission.
    /// If a later submission exists, it returns an error.
    async fn check_for_later_submissions(
        &self,
        payload: &impl BidSubmission,
        on_receive: u64,
    ) -> Result<(), BuilderApiError> {
        match self
            .auctioneer
            .get_builder_latest_payload_received_at(
                payload.slot().as_u64(),
                payload.builder_public_key(),
                payload.parent_hash(),
                payload.proposer_public_key(),
            )
            .await
        {
            Ok(Some(latest_payload_received_at)) => {
                if on_receive < latest_payload_received_at {
                    return Err(BuilderApiError::AlreadyProcessingNewerPayload);
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!(%err, "failed to get last slot delivered");
            }
        }
        Ok(())
    }
}
