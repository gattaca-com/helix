use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Extension,
};
use helix_common::{
    bid_submission::{BidSubmission, OptimisticVersion},
    metadata_provider::MetadataProvider,
    task,
    utils::{extract_request_id, utcnow_ns},
    SubmissionTrace,
};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use helix_types::SignedBidSubmission;
use tracing::{debug, error, info, trace, warn, Instrument};

use super::api::BuilderApi;
use crate::{
    builder::{
        api::{decode_payload, sanity_check_block_submission},
        error::BuilderApiError,
        v2_check::V2SubMessage,
    },
    Api, HEADER_API_KEY,
};

impl<A: Api> BuilderApi<A> {
    /// Handles the submission of a new block by performing various checks and verifications
    /// before saving the submission to the auctioneer. This is expected to pair with submit_header.
    ///
    /// 1. Receives the request and decodes the payload into a `SignedBidSubmission` object.
    /// 2. Validates the builder and checks against the next proposer duty.
    /// 3. Verifies the signature of the payload.
    /// 4. Runs further validations against auctioneer.
    /// 5. Simulates the block to validate the payment.
    /// 6. Saves the bid to auctioneer and db.
    ///
    /// Implements this API: https://docs.titanrelay.xyz/builders/builder-integration#optimistic-v2
    #[tracing::instrument(skip_all, fields(id =% extract_request_id(req.headers())))]
    pub async fn submit_block_v2(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        Extension(on_receive_ns): Extension<u64>,
        req: Request<Body>,
    ) -> Result<StatusCode, BuilderApiError> {
        let mut trace = SubmissionTrace {
            receive: on_receive_ns,
            read_body: utcnow_ns(),
            ..Default::default()
        };
        trace.metadata = api.metadata_provider.get_metadata(req.headers());

        let skip_sigverify =
            req.headers().get(HEADER_API_KEY).is_some_and(|key| api.auctioneer.check_api_key(key));
        // Decode the incoming request body into a payload
        let (parts, body) = req.into_parts();
        let (payload, _) = decode_payload(&parts.uri, &parts.headers, body, &mut trace).await?;

        tracing::Span::current().record("slot", payload.slot().as_u64() as i64);
        tracing::Span::current()
            .record("builder_pubkey", tracing::field::display(payload.builder_public_key()));
        tracing::Span::current()
            .record("block_hash", tracing::field::display(payload.message().block_hash));

        info!(
            decode_time = trace.decode.saturating_sub(trace.read_body),
            block_value = %payload.value(),
            "payload decoded",
        );

        Self::handle_optimistic_payload(api, payload, trace, OptimisticVersion::V2, skip_sigverify)
            .await
    }

    pub(crate) async fn handle_optimistic_payload(
        api: Arc<BuilderApi<A>>,
        payload: SignedBidSubmission,
        mut trace: SubmissionTrace,
        optimistic_version: OptimisticVersion,
        skip_sigverify: bool,
    ) -> Result<StatusCode, BuilderApiError> {
        let (head_slot, next_duty) = api.curr_slot_info.slot_info();

        let builder_pub_key = payload.builder_public_key().clone();
        let block_hash = payload.message().block_hash;

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

        payload.blobs_bundle().validate()?;
        trace!("validated blobs bundle");

        // Verify that we have a validator connected for this slot
        // Note: in `submit_block_v2` we have to do this check after decoding
        // so we can send a `PayloadReceived` message.
        let Some(next_duty) = next_duty else {
            warn!(?block_hash, "could not find slot duty");
            return Err(BuilderApiError::ProposerDutyNotFound);
        };

        // Fetch the next payload attributes and validate basic information
        let payload_attributes =
            api.fetch_payload_attributes(payload.slot(), *payload.parent_hash(), &block_hash)?;

        // Fetch builder info
        let builder_info = api.fetch_builder_info(payload.builder_public_key());

        // submit_block_v2 can only be processed optimistically.
        // Make sure that the builder has enough collateral to cover the submission.
        if let Err(err) = Self::check_builder_collateral(&payload, &builder_info) {
            warn!(%err, "builder has insufficient collateral");
            return Err(err);
        }

        // Discard any OptimisticV2 submissions if the proposer has regional filtering enabled
        // and the builder is not optimistic for regional filtering.
        if next_duty.entry.preferences.filtering.is_regional() &&
            !builder_info.can_process_regional_slot_optimistically()
        {
            warn!("proposer has regional filtering enabled, discarding {optimistic_version:?} submission");
            return Err(BuilderApiError::BuilderNotOptimistic {
                builder_pub_key: payload.builder_public_key().clone(),
            });
        }

        // Handle trusted builders check
        if !Self::check_if_trusted_builder(&next_duty, &builder_info) {
            let proposer_trusted_builders = next_duty.entry.preferences.trusted_builders.unwrap();
            warn!(
                builder_pub_key = ?payload.builder_public_key(),
                proposer_trusted_builders = ?proposer_trusted_builders,
                "builder not in proposer trusted builders list",
            );
            return Err(BuilderApiError::BuilderNotInProposersTrustedList {
                proposer_trusted_builders,
            });
        }

        // Verify payload has not already been delivered
        if let Some(slot) = api.auctioneer.get_last_slot_delivered() {
            if payload.slot() <= slot {
                debug!("payload already delivered");
                return Err(BuilderApiError::PayloadAlreadyDelivered);
            }
        }

        // Check for tx root against header received
        if let Err(err) = api.check_tx_root_against_header(&payload) {
            match err {
                // Could have just received the payload before the header
                BuilderApiError::MissingTransactionsRoot => {}
                // Nothing the builder can do about this error
                BuilderApiError::AuctioneerError(err) => {
                    return Err(BuilderApiError::AuctioneerError(err))
                }
                _ => {
                    api.demote_builder(
                        payload.slot().as_u64(),
                        &builder_pub_key,
                        &block_hash,
                        &err,
                    )
                    .await;
                    return Err(err);
                }
            }
        }

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
        trace.pre_checks = utcnow_ns();

        api.verify_signature(&payload, skip_sigverify, &mut trace)?;

        match api
            .simulate_submission(
                &payload,
                &builder_info,
                &mut trace,
                next_duty.entry,
                &payload_attributes,
            )
            .await
        {
            Ok(val) => val,
            Err(err) => {
                // Any invalid submission for optimistic v2/v3 results in a demotion.
                api.demote_builder(payload.slot().as_u64(), &builder_pub_key, &block_hash, &err)
                    .await;
                return Err(err);
            }
        };

        if optimistic_version == OptimisticVersion::V2 {
            if let Err(err) = api
                .v2_checks_tx
                .try_send(V2SubMessage::new_from_block_submission(&payload, trace.receive))
            {
                error!(%err, "failed to send block to v2 checker");
            }
        }

        // Save bid to auctioneer
        api.auctioneer.save_execution_payload(
            payload.slot().as_u64(),
            &payload.message().proposer_pubkey,
            payload.block_hash(),
            payload.payload_and_blobs_ref().to_owned(),
        );
        trace.auctioneer_update = utcnow_ns();

        api.auctioneer.save_bid_trace(payload.bid_trace());

        // Gossip to other relays
        if api.relay_config.payload_gossip_enabled {
            api.gossip_payload(&payload, payload.payload_and_blobs_ref()).await;
        }

        // Log some final info
        trace.request_finish = utcnow_ns();
        debug!(
            ?trace,
            request_duration_ns = trace.request_finish.saturating_sub(trace.receive),
            "sumbit_block_v2 request finished"
        );

        // Save submission to db
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

    fn check_tx_root_against_header(
        &self,
        payload: &SignedBidSubmission,
    ) -> Result<(), BuilderApiError> {
        match self.tx_root_cache.get(payload.block_hash()) {
            Some(expected_tx_root) => {
                let expected_tx_root = expected_tx_root.1;
                let tx_root = payload.transactions_root();
                if *expected_tx_root != tx_root {
                    warn!("tx root mismatch");

                    Err(BuilderApiError::TransactionsRootMismatch {
                        got: tx_root,
                        expected: expected_tx_root,
                    })
                } else {
                    Ok(())
                }
            }
            None => {
                warn!("no tx root found for block hash");

                Err(BuilderApiError::MissingTransactionsRoot)
            }
        }
    }
}
