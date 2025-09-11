use std::sync::Arc;

use axum::http::StatusCode;
use helix_common::{
    bid_submission::{BidSubmission, OptimisticVersion},
    task,
    utils::utcnow_ns,
    SubmissionTrace,
};
use helix_database::DatabaseService;
use helix_types::SignedBidSubmission;
use tracing::{debug, error, warn, Instrument};

use super::api::BuilderApi;
use crate::{
    builder::{api::sanity_check_block_submission, error::BuilderApiError},
    Api,
};

impl<A: Api> BuilderApi<A> {
    pub(crate) async fn handle_optimistic_payload(
        api: Arc<BuilderApi<A>>,
        payload: SignedBidSubmission,
        mut trace: SubmissionTrace,
        optimistic_version: OptimisticVersion,
        skip_sigverify: bool,
    ) -> Result<StatusCode, BuilderApiError> {
        let (head_slot, next_duty) = api.curr_slot_info.slot_info();

        let builder_pub_key = payload.builder_public_key();
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
                builder_pub_key: *payload.builder_public_key(),
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
                    api.demote_builder(payload.slot().as_u64(), builder_pub_key, &block_hash, &err)
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
                api.demote_builder(payload.slot().as_u64(), builder_pub_key, &block_hash, &err)
                    .await;
                return Err(err);
            }
        };

        // Save bid to auctioneer
        api.auctioneer.save_execution_payload(
            payload.slot().as_u64(),
            &payload.message().proposer_pubkey,
            payload.block_hash(),
            payload.payload_and_blobs_ref().to_owned(),
        );
        trace.auctioneer_update = utcnow_ns();

        // Gossip to other relays
        if api.relay_config.payload_gossip_enabled {
            api.gossip_payload(
                &payload,
                payload.payload_and_blobs_ref(),
                api.chain_info.current_fork_name(),
            )
            .await;
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
