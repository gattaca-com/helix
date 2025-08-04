use std::sync::Arc;

use alloy_primitives::Address;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Extension,
};
use bytes::Bytes;
use helix_common::{
    self,
    bid_sorter::BidSorterMessage,
    bid_submission::{BidSubmission, OptimisticVersion},
    metadata_provider::MetadataProvider,
    metrics::ApiMetrics,
    task,
    utils::{extract_request_id, utcnow_ns},
    SubmissionTrace,
};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use helix_types::{
    BlockMergingData, BlockMergingPreferences, MergeableBundle, MergeableBundles, Order,
    SignedBidSubmission,
};
use hyper::HeaderMap;
use tracing::{debug, error, info, trace, warn, Instrument, Level};

use super::api::BuilderApi;
use crate::{
    builder::{
        api::{decode_payload, sanity_check_block_submission},
        error::BuilderApiError,
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
        Extension(on_receive_ns): Extension<u64>,
        headers: HeaderMap,
        req: Request<Body>,
    ) -> Result<StatusCode, BuilderApiError> {
        trace!("new block submission");

        let mut trace = SubmissionTrace { receive: on_receive_ns, ..Default::default() };
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
        ApiMetrics::cancellable_bid(is_cancellations_enabled);

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
        let floor_bid_value = api.get_current_floor(payload.slot());
        if payload.value() <= floor_bid_value {
            return Err(BuilderApiError::BidBelowFloor);
        }
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
                &payload,
                next_duty,
                &builder_info,
                &mut trace,
                &payload_attributes,
            )
            .await?;

        let optimistic_version = if was_simulated_optimistically {
            OptimisticVersion::V1
        } else {
            OptimisticVersion::NotOptimistic
        };
        trace!(is_optimistic = was_simulated_optimistically, "verified submitted block");

        let mergeable_bundles = get_mergeable_bundles(&payload, payload.merging_data())?;

        if let Err(err) = api.sorter_tx.try_send(BidSorterMessage::new_from_block_submission(
            &payload,
            &trace,
            optimistic_version,
            is_cancellations_enabled,
            mergeable_bundles,
        )) {
            error!(?err, "failed to send submission to sorter");
            return Err(BuilderApiError::InternalError);
        };
        trace!("sent bid to bid sorter");

        // Save the execution payload
        // TODO: if this and similar other calls fail we should stop serving headers and send an
        // alert, not much we can do
        api.auctioneer
            .save_execution_payload(
                payload.slot().as_u64(),
                payload.proposer_public_key(),
                payload.block_hash(),
                payload.payload_and_blobs_ref(),
            )
            .await?;
        trace.auctioneer_update = utcnow_ns();
        trace!("saved payload to redis");

        if let Err(err) = api.auctioneer.save_bid_trace(payload.bid_trace()).await {
            error!(%err, "failed to save bid trace");
            return Err(BuilderApiError::AuctioneerError(err));
        }
        trace!("saved bid trace to redis");

        // TODO: validate merging data
        let merging_preferences =
            BlockMergingPreferences { allow_appending: payload.merging_data().allow_appending };

        if let Err(err) = api
            .auctioneer
            .save_block_merging_preferences(
                payload.slot().as_u64(),
                payload.proposer_public_key(),
                payload.block_hash(),
                &merging_preferences,
            )
            .await
        {
            error!(%err, "failed to save block merging data");
            return Err(BuilderApiError::AuctioneerError(err));
        }
        trace!("saved block merging data to redis");

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

fn get_mergeable_bundles(
    payload: &SignedBidSubmission,
    merging_data: &BlockMergingData,
) -> Result<MergeableBundles, BuilderApiError> {
    let execution_payload = payload.execution_payload_ref();
    let txs = execution_payload.transactions();
    let mergeable_bundles = merging_data
        .merge_orders
        .iter()
        .map(|order| match order {
            Order::Tx(tx) => {
                let raw_tx = txs
                    .get(tx.index as usize)
                    .cloned()
                    // TODO: use a better error
                    .ok_or(BuilderApiError::MissingTransactions)?;
                let reverting_txs = if tx.can_revert { vec![0] } else { vec![] };

                Ok(MergeableBundle {
                    transactions: vec![Bytes::from_owner(raw_tx.to_vec())],
                    reverting_txs,
                    dropping_txs: vec![],
                })
            }
            Order::Bundle(bundle) => {
                let transactions = bundle
                    .txs
                    .iter()
                    .map(|tx_index| {
                        let raw_tx = txs
                            .get(*tx_index)
                            .cloned()
                            // TODO: use a better error
                            .ok_or(BuilderApiError::MissingTransactions)?;

                        Ok(Bytes::from_owner(raw_tx.to_vec()))
                    })
                    .collect::<Result<_, BuilderApiError>>()?;

                let reverting_txs = bundle
                    .txs
                    .iter()
                    .enumerate()
                    .filter(|(_, tx_index)| bundle.reverting_txs.contains(tx_index))
                    .map(|(i, _)| i)
                    .collect();
                let dropping_txs = bundle
                    .txs
                    .iter()
                    .enumerate()
                    .filter(|(_, tx_index)| bundle.dropping_txs.contains(tx_index))
                    .map(|(i, _)| i)
                    .collect();

                Ok(MergeableBundle { transactions, reverting_txs, dropping_txs })
            }
        })
        .collect::<Result<Vec<_>, BuilderApiError>>()?;

    // TODO: which address should we use here?
    Ok(MergeableBundles::new(Address::default(), mergeable_bundles))
}
