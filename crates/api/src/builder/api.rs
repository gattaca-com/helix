use std::{
    collections::HashMap,
    io::Read,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{to_bytes, Body},
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use ethereum_consensus::{
    configs::mainnet::{CAPELLA_FORK_EPOCH, SECONDS_PER_SLOT},
    phase0::mainnet::SLOTS_PER_EPOCH,
    primitives::{BlsPublicKey, Hash32},
    ssz::{self, prelude::*},
};
use flate2::read::GzDecoder;
use futures::StreamExt;
use hyper::HeaderMap;
use tokio::{
    sync::{
        mpsc::{self, error::SendError, Receiver, Sender},
        RwLock,
    },
    time::{self, Instant},
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use helix_common::{
    api::{
        builder_api::{BuilderGetValidatorsResponse, BuilderGetValidatorsResponseEntry},
        proposer_api::ValidatorRegistrationInfo,
    },
    bid_submission::{
        cancellation::SignedCancellation, v2::header_submission::SignedHeaderSubmission,
        BidSubmission, BidTrace, SignedBidSubmission,
    },
    chain_info::ChainInfo,
    signing::RelaySigningContext,
    simulator::BlockSimError,
    versioned_payload::PayloadAndBlobs,
    BuilderInfo, GossipedHeaderTrace, GossipedPayloadTrace, HeaderSubmissionTrace, RelayConfig,
    SignedBuilderBid, SubmissionTrace,
};
use helix_database::DatabaseService;
use helix_datastore::{types::SaveBidAndUpdateTopBidResponse, Auctioneer};
use helix_housekeeper::{ChainUpdate, PayloadAttributesUpdate, SlotUpdate};
use helix_utils::{get_payload_attributes_key, has_reached_fork};

use crate::{
    builder::{
        error::BuilderApiError, traits::BlockSimulator, BlockSimRequest, DbInfo, OptimisticVersion,
    },
    gossiper::{
        traits::GossipClientTrait,
        types::{
            broadcast_cancellation::BroadcastCancellationParams, BroadcastHeaderParams,
            BroadcastPayloadParams, GossipedMessage,
        },
    },
};

pub(crate) const MAX_PAYLOAD_LENGTH: usize = 1024 * 1024 * 10;

#[derive(Clone)]
pub struct BuilderApi<A, DB, S, G>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
    S: BlockSimulator + 'static,
    G: GossipClientTrait + 'static,
{
    auctioneer: Arc<A>,
    db: Arc<DB>,
    chain_info: Arc<ChainInfo>,
    simulator: S,
    gossiper: Arc<G>,
    signing_context: Arc<RelaySigningContext>,
    relay_config: Arc<RelayConfig>,
    db_sender: Sender<DbInfo>,

    /// Information about the current head slot and next proposer duty
    curr_slot_info: Arc<RwLock<(u64, Option<BuilderGetValidatorsResponseEntry>)>>,

    proposer_duties_response: Arc<RwLock<Option<Vec<u8>>>>,
    payload_attributes: Arc<RwLock<HashMap<String, PayloadAttributesUpdate>>>,
}

impl<A, DB, S, G> BuilderApi<A, DB, S, G>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
    S: BlockSimulator + 'static,
    G: GossipClientTrait + 'static,
{
    pub fn new(
        auctioneer: Arc<A>,
        db: Arc<DB>,
        chain_info: Arc<ChainInfo>,
        simulator: S,
        gossiper: Arc<G>,
        signing_context: Arc<RelaySigningContext>,
        relay_config: RelayConfig,
        slot_update_subscription: Sender<Sender<ChainUpdate>>,
        gossip_receiver: Receiver<GossipedMessage>,
    ) -> Self {
        let (db_sender, db_receiver) = mpsc::channel::<DbInfo>(10_000);

        // Spin up db processing task
        let db_clone = db.clone();
        tokio::spawn(async move {
            process_db_additions(db_clone, db_receiver).await;
        });

        let api = Self {
            auctioneer,
            db,
            chain_info,
            simulator,
            gossiper,
            signing_context,
            relay_config: Arc::new(relay_config),

            db_sender,

            curr_slot_info: Arc::new(RwLock::new((0, None))),
            proposer_duties_response: Arc::new(RwLock::new(None)),
            payload_attributes: Arc::new(RwLock::new(HashMap::new())),
        };

        // Spin up gossip processing task
        let api_clone = api.clone();
        tokio::spawn(async move {
            api_clone.process_gossiped_info(gossip_receiver).await;
        });

        // Spin up the housekeep task.
        // This keeps the curr slot info variables up to date.
        let api_clone = api.clone();
        tokio::spawn(async move {
            if let Err(err) = api_clone.housekeep(slot_update_subscription.clone()).await {
                error!(
                    error = %err,
                    "BuilderApi. housekeep task encountered an error",
                );
            }
        });

        api
    }

    /// Implements this API: <https://flashbots.github.io/relay-specs/#/Builder/getValidators>
    pub async fn get_validators(
        Extension(api): Extension<Arc<BuilderApi<A, DB, S, G>>>,
    ) -> impl IntoResponse {
        let duty_bytes = api.proposer_duties_response.read().await.clone();
        match duty_bytes {
            Some(bytes) => Response::builder()
                .status(StatusCode::OK)
                .body(axum::body::Body::from(bytes.clone()))
                .unwrap()
                .into_response(),
            None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }

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
    pub async fn submit_block(
        Extension(api): Extension<Arc<BuilderApi<A, DB, S, G>>>,
        req: Request<Body>,
    ) -> Result<StatusCode, BuilderApiError> {
        let request_id = Uuid::new_v4();
        let mut trace = SubmissionTrace { receive: get_nanos_timestamp()?, ..Default::default() };
        let (head_slot, next_duty) = api.curr_slot_info.read().await.clone();

        info!(
            request_id = %request_id,
            event = "submit_block",
            head_slot = head_slot,
            timestamp_request_start = trace.receive,
        );

        // Decode the incoming request body into a payload
        let (payload, is_cancellations_enabled) =
            decode_payload(req, &mut trace, &request_id).await?;
        let block_hash = payload.message().block_hash.clone();

        // Verify that we have a validator connected for this slot
        if next_duty.is_none() {
            warn!(request_id = %request_id, "could not find slot duty");
            return Err(BuilderApiError::ProposerDutyNotFound)
        }
        let next_duty = next_duty.unwrap();

        debug!(
            request_id = %request_id,
            builder_pub_key = ?payload.builder_public_key(),
            block_value = %payload.value(),
            block_hash = ?block_hash,
            "payload decoded",
        );

        // Verify the payload is for the current slot
        if payload.slot() <= head_slot {
            warn!(
                request_id = %request_id,
                "submission is for a past slot",
            );
            return Err(BuilderApiError::SubmissionForPastSlot {
                current_slot: head_slot,
                submission_slot: payload.slot(),
            })
        }

        // Fetch the next payload attributes and validate basic information
        let payload_attributes = api
            .fetch_payload_attributes(payload.slot(), payload.parent_hash(), &request_id)
            .await?;

        // Handle duplicates.
        if let Err(err) = api
            .check_for_duplicate_block_hash(
                &block_hash,
                payload.slot(),
                payload.parent_hash(),
                payload.proposer_public_key(),
                &request_id,
            )
            .await
        {
            match err {
                BuilderApiError::DuplicateBlockHash { block_hash } => {
                    // We dont return the error here as we want to continue processing the request.
                    // This mitigates the risk of someone sending an invalid payload
                    // with a valid header, which would block subsequent submissions with the same
                    // header and valid payload.
                    debug!(
                        request_id = %request_id,
                        block_hash = ?block_hash,
                        builder_pub_key = ?payload.builder_public_key(),
                        "block hash already seen"
                    );
                }
                _ => return Err(err),
            }
        }

        // Verify the payload value is above the floor bid
        let floor_bid_value = api
            .check_if_bid_is_below_floor(
                payload.slot(),
                payload.parent_hash(),
                payload.proposer_public_key(),
                payload.builder_public_key(),
                payload.value(),
                is_cancellations_enabled,
                &request_id,
            )
            .await?;
        trace.floor_bid_checks = get_nanos_timestamp()?;

        // Fetch builder info
        let builder_info = api.fetch_builder_info(payload.builder_public_key()).await;

        // Handle trusted builders check
        if !api.check_if_trusted_builder(&next_duty, &builder_info).await {
            let proposer_trusted_builders = next_duty.entry.preferences.trusted_builders.unwrap();
            warn!(
                request_id = %request_id,
                builder_pub_key = ?payload.builder_public_key(),
                proposer_trusted_builders = ?proposer_trusted_builders,
                "builder not in proposer trusted builders list",
            );
            return Err(BuilderApiError::BuilderNotInProposersTrustedList {
                proposer_trusted_builders,
            })
        }

        // Verify payload has not already been delivered
        match api.auctioneer.get_last_slot_delivered().await {
            Ok(Some(slot)) => {
                if payload.slot() <= slot {
                    warn!(request_id = %request_id, "payload already delivered");
                    return Err(BuilderApiError::PayloadAlreadyDelivered)
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!(request_id = %request_id, error = %err, "failed to get last slot delivered");
            }
        }

        // Sanity check the payload
        if let Err(err) = sanity_check_block_submission(
            &payload,
            payload.bid_trace(),
            &next_duty,
            &payload_attributes,
            &api.chain_info,
        ) {
            warn!(request_id = %request_id, error = %err, "failed sanity check");
            return Err(err)
        }
        trace.pre_checks = get_nanos_timestamp()?;

        let (payload, was_simulated_optimistically) = api
            .verify_submitted_block(
                payload,
                next_duty,
                &builder_info,
                &mut trace,
                &request_id,
                &payload_attributes,
            )
            .await?;

        // If cancellations are enabled, then abort now if there is a later submission
        if is_cancellations_enabled {
            if let Err(err) =
                api.check_for_later_submissions(&payload, trace.receive, &request_id).await
            {
                warn!(request_id = %request_id, error = %err, "already processing later submission");
                return Err(err)
            }
        }

        // Save bid to auctioneer
        match api
            .save_bid_to_auctioneer(
                &payload,
                &mut trace,
                is_cancellations_enabled,
                floor_bid_value,
                &request_id,
            )
            .await?
        {
            // If the bid was succesfully saved then we gossip the header and payload to all other
            // relays.
            Some((builder_bid, execution_payload)) => {
                api.gossip_new_submission(
                    &payload,
                    execution_payload,
                    builder_bid,
                    is_cancellations_enabled,
                    trace.receive,
                    &request_id,
                )
                .await;
            }
            None => { /* Bid wasn't saved so no need to gossip as it will never be served */ }
        }

        // Log some final info
        trace.request_finish = get_nanos_timestamp()?;
        info!(
            request_id = %request_id,
            trace = ?trace,
            request_duration_ns = trace.request_finish.saturating_sub(trace.receive),
            "submit_block request finished"
        );

        let optimistic_version = if was_simulated_optimistically {
            OptimisticVersion::V1
        } else {
            OptimisticVersion::NotOptimistic
        };

        // Save submission to db.
        tokio::spawn(async move {
            if let Err(err) = api
                .db
                .store_block_submission(payload, Arc::new(trace), optimistic_version as i16)
                .await
            {
                error!(
                    error = %err,
                    "failed to store block submission",
                )
            }
        });

        Ok(StatusCode::OK)
    }

    /// Handles the submission of a new payload header by performing various checks and
    /// verifications before saving the headre to the auctioneer.
    pub async fn submit_header(
        Extension(api): Extension<Arc<BuilderApi<A, DB, S, G>>>,
        req: Request<Body>,
    ) -> Result<StatusCode, BuilderApiError> {
        let request_id = Uuid::new_v4();
        let mut trace =
            HeaderSubmissionTrace { receive: get_nanos_timestamp()?, ..Default::default() };
        let (head_slot, next_duty) = api.curr_slot_info.read().await.clone();

        info!(
            request_id = %request_id,
            event = "submit_header",
            head_slot = head_slot,
            timestamp_request_start = trace.receive,
        );

        // Decode the incoming request body into a payload
        let (mut payload, is_cancellations_enabled) =
            decode_header_submission(req, &mut trace, &request_id).await?;
        let block_hash = payload.block_hash().clone();

        // Verify that we have a validator connected for this slot
        if next_duty.is_none() {
            warn!(request_id = %request_id, "could not find slot duty");
            return Err(BuilderApiError::ProposerDutyNotFound)
        }
        let next_duty = next_duty.unwrap();

        debug!(
            request_id = %request_id,
            builder_pub_key = ?payload.builder_public_key(),
            block_value = %payload.value(),
            block_hash = ?block_hash,
            "header submission decoded",
        );

        // Verify the payload is for the current slot
        if payload.slot() <= head_slot {
            warn!(
                request_id = %request_id,
                "submission is for a past slot",
            );
            return Err(BuilderApiError::SubmissionForPastSlot {
                current_slot: head_slot,
                submission_slot: payload.slot(),
            })
        }

        // Fetch the next payload attributes and validate basic information
        let payload_attributes = api
            .fetch_payload_attributes(payload.slot(), payload.parent_hash(), &request_id)
            .await?;

        // Fetch builder info
        let builder_info = api.fetch_builder_info(payload.builder_public_key()).await;

        // Submit header can only be processed optimistically.
        // Make sure that the builder has enough collateral to cover the submission.
        if let Err(err) = api.check_builder_collateral(&payload, &builder_info, &request_id).await {
            warn!(request_id = %request_id, error = %err, "builder has insufficient collateral");
            return Err(err)
        }

        // Handle duplicates.
        if let Err(err) = api
            .check_for_duplicate_block_hash(
                &block_hash,
                payload.slot(),
                payload.parent_hash(),
                payload.proposer_public_key(),
                &request_id,
            )
            .await
        {
            match err {
                BuilderApiError::DuplicateBlockHash { block_hash } => {
                    // We dont return the error here as we want to continue processing the request.
                    // This mitigates the risk of someone sending an invalid payload
                    // with a valid header, which would block subsequent submissions with the same
                    // header and valid payload.
                    debug!(
                        request_id = %request_id,
                        block_hash = ?block_hash,
                        builder_pub_key = ?payload.builder_public_key(),
                        "block hash already seen"
                    );
                }
                _ => return Err(err),
            }
        }

        // Discard any OptimisticV2 submissions if the proposer has regional filtering enabled
        if next_duty.entry.preferences.filtering.is_regional() {
            warn!(request_id = %request_id, "proposer has regional filtering, discarding optimistic v2 submission");
            return Err(BuilderApiError::V2SubmissionsInvalidIfProposerRequiresRegionalFiltering)
        }

        // Validate basic information about the payload
        if let Err(err) = sanity_check_block_submission(
            &payload,
            payload.bid_trace(),
            &next_duty,
            &payload_attributes,
            &api.chain_info,
        ) {
            warn!(request_id = %request_id, error = %err, "failed sanity check");
            return Err(err)
        }

        // Handle trusted builders check
        if !api.check_if_trusted_builder(&next_duty, &builder_info).await {
            let proposer_trusted_builders = next_duty.entry.preferences.trusted_builders.unwrap();
            warn!(
                request_id = %request_id,
                builder_pub_key = ?payload.builder_public_key(),
                proposer_trusted_builders = ?proposer_trusted_builders,
                "builder not in proposer trusted builders list",
            );
            return Err(BuilderApiError::BuilderNotInProposersTrustedList {
                proposer_trusted_builders,
            })
        }

        trace.pre_checks = get_nanos_timestamp()?;

        // Verify the payload signature
        if let Err(err) = payload.verify_signature(&api.chain_info.context) {
            warn!(request_id = %request_id, error = %err, "failed to verify signature");
            return Err(BuilderApiError::SignatureVerificationFailed)
        }
        trace.signature = get_nanos_timestamp()?;

        // Verify payload has not already been delivered
        match api.auctioneer.get_last_slot_delivered().await {
            Ok(Some(slot)) => {
                if payload.slot() <= slot {
                    warn!(request_id = %request_id, "payload already delivered");
                    return Err(BuilderApiError::PayloadAlreadyDelivered)
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!(request_id = %request_id, error = %err, "failed to get last slot delivered");
            }
        }

        let payload = Arc::new(payload);

        // Verify the payload value is above the floor bid
        let floor_bid_value = api
            .check_if_bid_is_below_floor(
                payload.slot(),
                payload.parent_hash(),
                payload.proposer_public_key(),
                payload.builder_public_key(),
                payload.value(),
                is_cancellations_enabled,
                &request_id,
            )
            .await?;
        trace.floor_bid_checks = get_nanos_timestamp()?;

        // Save bid to auctioneer
        match api
            .save_header_bid_to_auctioneer(
                payload.clone(),
                &mut trace,
                is_cancellations_enabled,
                floor_bid_value,
                &request_id,
            )
            .await?
        {
            Some(builder_bid) => {
                api.gossip_header(
                    builder_bid,
                    payload.bid_trace(),
                    is_cancellations_enabled,
                    trace.receive,
                    &request_id,
                )
                .await;
            }
            None => { /* Bid wasn't saved so no need to gossip as it will never be served */ }
        }

        // Log some final info
        trace.request_finish = get_nanos_timestamp()?;
        info!(
            request_id = %request_id,
            trace = ?trace,
            request_duration_ns = trace.request_finish.saturating_sub(trace.receive),
            "submit_header request finished"
        );

        // Save pending block header to auctioneer
        api.auctioneer
            .save_pending_block_header(
                payload.slot(),
                payload.builder_public_key(),
                payload.block_hash(),
                trace.receive / 1_000_000, // convert to ms
            ).await
            .map_err(|err| {
                error!(request_id = %request_id, error = %err, "failed to save pending block header");
                BuilderApiError::AuctioneerError(err)
            })?;

        // Save submission to db
        let db = api.db.clone();
        tokio::spawn(async move {
            if let Err(err) = db.store_header_submission(payload, Arc::new(trace)).await {
                error!(
                    error = %err,
                    "failed to store header submission",
                )
            }
        });

        Ok(StatusCode::OK)
    }

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
    /// Implements this API: TODO: point to gattaca spec. rename?
    pub async fn submit_block_v2(
        Extension(api): Extension<Arc<BuilderApi<A, DB, S, G>>>,
        req: Request<Body>,
    ) -> Result<StatusCode, BuilderApiError> {
        let request_id = Uuid::new_v4();
        let now = SystemTime::now();
        let mut trace = SubmissionTrace { receive: get_nanos_from(now)?, ..Default::default() };
        let (head_slot, next_duty) = api.curr_slot_info.read().await.clone();

        info!(
            request_id = %request_id,
            event = "submit_block_v2",
            head_slot = head_slot,
            timestamp_request_start = trace.receive,
        );

        // Decode the incoming request body into a payload
        let (payload, _) = decode_payload(req, &mut trace, &request_id).await?;

        let builder_pub_key = payload.builder_public_key().clone();
        let block_hash = payload.message().block_hash.clone();
        debug!(
            request_id = %request_id,
            builder_pub_key = ?builder_pub_key,
            block_value = %payload.value(),
            block_hash = ?payload.block_hash(),
            "payload decoded",
        );

        // Save pending block payload to auctioneer
        api.auctioneer
            .save_pending_block_payload(
                payload.slot(),
                payload.builder_public_key(),
                payload.block_hash(),
                trace.receive / 1_000_000, // convert to ms
            ).await
            .map_err(|err| {
                error!(request_id = %request_id, error = %err, "failed to save pending block header");
                BuilderApiError::AuctioneerError(err)
            })?;

        // Verify the payload is for the current slot
        if payload.slot() <= head_slot {
            warn!(
                request_id = %request_id,
                "submission is for a past slot",
            );
            return Err(BuilderApiError::SubmissionForPastSlot {
                current_slot: head_slot,
                submission_slot: payload.slot(),
            })
        }

        // Verify that we have a validator connected for this slot
        // Note: in `submit_block_v2` we have to do this check after decoding
        // so we can send a `PayloadReceived` message.
        if next_duty.is_none() {
            warn!(request_id = %request_id, "could not find slot duty");
            return Err(BuilderApiError::ProposerDutyNotFound)
        }
        let next_duty = next_duty.unwrap();

        // Fetch the next payload attributes and validate basic information
        let payload_attributes = api
            .fetch_payload_attributes(payload.slot(), payload.parent_hash(), &request_id)
            .await?;

        // Fetch builder info
        let builder_info = api.fetch_builder_info(payload.builder_public_key()).await;

        // submit_block_v2 can only be processed optimistically.
        // Make sure that the builder has enough collateral to cover the submission.
        if let Err(err) = api.check_builder_collateral(&payload, &builder_info, &request_id).await {
            warn!(request_id = %request_id, error = %err, "builder has insufficient collateral");
            return Err(err)
        }

        // Discard any OptimisticV2 submissions if the proposer has regional filtering enabled
        if next_duty.entry.preferences.filtering.is_regional() {
            warn!(request_id = %request_id, "proposer has regional filtering enabled, discarding optimistic v2 submission");
            return Err(BuilderApiError::V2SubmissionsInvalidIfProposerRequiresRegionalFiltering)
        }

        // Handle trusted builders check
        if !api.check_if_trusted_builder(&next_duty, &builder_info).await {
            let proposer_trusted_builders = next_duty.entry.preferences.trusted_builders.unwrap();
            warn!(
                request_id = %request_id,
                builder_pub_key = ?payload.builder_public_key(),
                proposer_trusted_builders = ?proposer_trusted_builders,
                "builder not in proposer trusted builders list",
            );
            return Err(BuilderApiError::BuilderNotInProposersTrustedList {
                proposer_trusted_builders,
            })
        }

        // Verify payload has not already been delivered
        match api.auctioneer.get_last_slot_delivered().await {
            Ok(Some(slot)) => {
                if payload.slot() <= slot {
                    warn!(request_id = %request_id, "payload already delivered");
                    return Err(BuilderApiError::PayloadAlreadyDelivered)
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!(request_id = %request_id, error = %err, "failed to get last slot delivered");
            }
        }

        // Check for tx root against header received
        if let Err(err) = api.check_tx_root_against_header(&payload, &request_id).await {
            match err {
                // Could have just received the payload before the header
                BuilderApiError::MissingTransactionsRoot => {}
                // Nothing the builder can do about this error
                BuilderApiError::AuctioneerError(err) => {
                    return Err(BuilderApiError::AuctioneerError(err))
                }
                _ => {
                    api.demote_builder(&builder_pub_key, &block_hash, &err, &request_id).await;
                    return Err(err)
                }
            }
        }

        // Sanity check the payload
        if let Err(err) = sanity_check_block_submission(
            &payload,
            payload.bid_trace(),
            &next_duty,
            &payload_attributes,
            &api.chain_info,
        ) {
            warn!(request_id = %request_id, error = %err, "failed sanity check");
            return Err(err)
        }
        trace.pre_checks = get_nanos_timestamp()?;

        let (payload, _) = match api
            .verify_submitted_block(
                payload,
                next_duty,
                &builder_info,
                &mut trace,
                &request_id,
                &payload_attributes,
            )
            .await
        {
            Ok(val) => val,
            Err(err) => {
                // Any invalid submission for optimistic v2 results in a demotion.
                api.demote_builder(&builder_pub_key, &block_hash, &err, &request_id).await;
                return Err(err)
            }
        };

        // Save bid to auctioneer
        if let Err(err) = api
            .auctioneer
            .save_execution_payload(
                payload.slot(),
                &payload.message().proposer_public_key,
                payload.block_hash(),
                &payload.payload_and_blobs(),
            )
            .await
        {
            error!(request_id = %request_id, error = %err, "failed to save execution payload");
            return Err(BuilderApiError::AuctioneerError(err))
        }
        trace.auctioneer_update = get_nanos_timestamp()?;

        // Gossip to other relays
        api.gossip_payload(&payload, payload.payload_and_blobs(), &request_id).await;

        // Log some final info
        trace.request_finish = get_nanos_timestamp()?;
        info!(
            request_id = %request_id,
            trace = ?trace,
            request_duration_ns = trace.request_finish.saturating_sub(trace.receive),
            "sumbit_block_v2 request finished"
        );

        // Save submission to db
        tokio::spawn(async move {
            if let Err(err) = api
                .db
                .store_block_submission(payload, Arc::new(trace), OptimisticVersion::V2 as i16)
                .await
            {
                error!(
                    error = %err,
                    "failed to store block submission",
                )
            }
        });

        Ok(StatusCode::OK)
    }

    /// Handles the cancellation of a bid for a builder. Builders currently cached bid in the
    /// auctioneer is deleted, the top bid is recalculated, and the cancellation is gossiped to
    /// all other relays.
    pub async fn cancel_bid(
        Extension(api): Extension<Arc<BuilderApi<A, DB, S, G>>>,
        Json(mut signed_cancellation): Json<SignedCancellation>,
    ) -> Result<StatusCode, BuilderApiError> {
        let request_id = Uuid::new_v4();
        let (head_slot, _next_duty) = api.curr_slot_info.read().await.clone();

        let slot = signed_cancellation.message.slot;

        info!(
            request_id = %request_id,
            event = "cancel_bid",
            head_slot = head_slot,
        );

        // Verify the cancellation is for the current slot
        if slot <= head_slot {
            warn!(
                request_id = %request_id,
                "cancellation is for a past slot",
            );
        }

        // Verify the payload signature
        if let Err(err) = signed_cancellation.verify_signature(&api.chain_info.context) {
            warn!(request_id = %request_id, error = %err, "failed to verify signature");
            return Err(BuilderApiError::SignatureVerificationFailed)
        }

        // Verify payload has not already been delivered
        match api.auctioneer.get_last_slot_delivered().await {
            Ok(Some(del_slot)) => {
                if slot <= del_slot {
                    warn!(request_id = %request_id, "payload already delivered");
                    return Err(BuilderApiError::PayloadAlreadyDelivered)
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!(request_id = %request_id, error = %err, "failed to get last slot delivered");
            }
        }

        if let Err(err) = api
            .auctioneer
            .delete_builder_bid(
                slot,
                &signed_cancellation.message.parent_hash,
                &signed_cancellation.message.proposer_public_key,
                &signed_cancellation.message.builder_public_key,
            )
            .await
        {
            error!(
                request_id = %request_id,
                error = %err,
                "Failed processing cancellable bid below floor. Could not delete builder bid.",
            );
            return Err(BuilderApiError::InternalError)
        }

        api.gossip_cancellation(signed_cancellation, &request_id).await;

        Ok(StatusCode::OK)
    }

    pub async fn get_top_bid(
        Extension(api): Extension<Arc<BuilderApi<A, DB, S, G>>>,
        headers: HeaderMap,
        ws: WebSocketUpgrade,
    ) -> Result<impl IntoResponse, BuilderApiError> {
        let api_key = headers.get("x-api-key").and_then(|key| key.to_str().ok());
        match api_key {
            Some(key) => match api.db.check_builder_api_key(key).await {
                Ok(true) => {}
                Ok(false) => return Err(BuilderApiError::InvalidApiKey),
                Err(err) => {
                    error!(error = %err, "failed to check api key");
                    return Err(BuilderApiError::InternalError)
                }
            },
            None => return Err(BuilderApiError::InvalidApiKey),
        }

        Ok(ws.on_upgrade(move |socket| push_top_bids(socket, api.auctioneer.clone())))
    }
}

// Handle Gossiped Payloads
impl<A, DB, S, G> BuilderApi<A, DB, S, G>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
    S: BlockSimulator + 'static,
    G: GossipClientTrait + 'static,
{
    pub async fn process_gossiped_header(&self, req: BroadcastHeaderParams) {
        let request_id = Uuid::new_v4();
        info!(
            request_id = %request_id,
            block_hash = ?req.signed_builder_bid.block_hash(),
            "received gossiped header",
        );

        let mut trace = GossipedHeaderTrace {
            on_receive: req.on_receive,
            on_gossip_receive: get_nanos_timestamp().unwrap_or_default(),
            ..Default::default()
        };

        // Verify that the gossiped header is not for a past slot
        let (head_slot, _) = self.curr_slot_info.read().await.clone();
        if req.slot <= head_slot {
            warn!(
                request_id = %request_id,
                "received gossiped header for a past slot",
            );
            return
        }

        // Handle duplicates.
        if self
            .check_for_duplicate_block_hash(
                req.signed_builder_bid.block_hash(),
                req.slot,
                &req.parent_hash,
                &req.proposer_pub_key,
                &request_id,
            )
            .await
            .is_err()
        {
            return
        }

        // Verify payload has not already been delivered
        match self.auctioneer.get_last_slot_delivered().await {
            Ok(Some(slot)) => {
                if req.slot <= slot {
                    warn!(request_id = %request_id, "payload already delivered");
                    return
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!(request_id = %request_id, error = %err, "failed to get last slot delivered");
            }
        }

        // Verify the bid value is above the floor bid
        let floor_bid_value = match self
            .check_if_bid_is_below_floor(
                req.slot,
                &req.parent_hash,
                &req.proposer_pub_key,
                &req.builder_pub_key,
                req.signed_builder_bid.value(),
                req.is_cancellations_enabled,
                &request_id,
            )
            .await
        {
            Ok(floor_bid_value) => floor_bid_value,
            Err(err) => {
                warn!(request_id = %request_id, error = %err, "bid is below floor");
                return
            }
        };

        trace.pre_checks = get_nanos_timestamp().unwrap_or_default();

        // Save header to auctioneer
        let mut update_bid_result = SaveBidAndUpdateTopBidResponse::default();
        if let Err(err) = self
            .auctioneer
            .save_signed_builder_bid_and_update_top_bid(
                &req.signed_builder_bid,
                &req.bid_trace,
                req.on_receive.into(),
                req.is_cancellations_enabled,
                floor_bid_value,
                &mut update_bid_result,
            )
            .await
        {
            warn!(request_id = %request_id, error = %err, "failed to save header bid");
            return
        }

        trace.auctioneer_update = get_nanos_timestamp().unwrap_or_default();

        info!(request_id = %request_id, "succesfully saved gossiped header");

        // Save latency trace to db
        let db = self.db.clone();
        tokio::spawn(async move {
            if let Err(err) = db
                .save_gossiped_header_trace(req.bid_trace.block_hash.clone(), Arc::new(trace))
                .await
            {
                error!(
                    error = %err,
                    "failed to store gossiped header trace",
                )
            }
        });
    }

    pub async fn process_gossiped_payload(&self, req: BroadcastPayloadParams) {
        let request_id = Uuid::new_v4();
        info!(
            request_id = %request_id,
            block_hash = ?req.execution_payload.execution_payload.block_hash(),
            "received gossiped payload",
        );

        let mut trace = GossipedPayloadTrace {
            receive: get_nanos_timestamp().unwrap_or_default(),
            ..Default::default()
        };

        // Save gossiped payload to auctioneer in case it was sent to diffent region than the header
        if let Err(err) = self
            .auctioneer
            .save_pending_block_payload(
                req.slot,
                &req.proposer_pub_key,
                &req.execution_payload.execution_payload.block_hash().clone(),
                trace.receive / 1_000_000, // convert to ms
            )
            .await
        {
            error!(request_id = %request_id, error = %err, "failed to save pending block header");
        }

        // Verify that the gossiped payload is not for a past slot
        let (head_slot, _) = self.curr_slot_info.read().await.clone();
        if req.slot <= head_slot {
            warn!(
                request_id = %request_id,
                "received gossiped payload for a past slot",
            );
            return
        }

        // Verify payload has not already been delivered
        match self.auctioneer.get_last_slot_delivered().await {
            Ok(Some(slot)) => {
                if req.slot <= slot {
                    warn!(request_id = %request_id, "payload already delivered");
                    return
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!(request_id = %request_id, error = %err, "failed to get last slot delivered");
            }
        }

        trace.pre_checks = get_nanos_timestamp().unwrap_or_default();

        // Save payload to auctioneer
        if let Err(err) = self
            .auctioneer
            .save_execution_payload(
                req.slot,
                &req.proposer_pub_key,
                req.execution_payload.execution_payload.block_hash(),
                &req.execution_payload,
            )
            .await
        {
            error!(request_id = %request_id, error = %err, "failed to save execution payload");
            return
        }

        trace.auctioneer_update = get_nanos_timestamp().unwrap_or_default();

        info!(request_id = %request_id, "succesfully saved gossiped payload");

        // Save gossiped payload trace to db
        let db = self.db.clone();
        tokio::spawn(async move {
            if let Err(err) = db
                .save_gossiped_payload_trace(
                    req.execution_payload.execution_payload.block_hash().clone(),
                    Arc::new(trace),
                )
                .await
            {
                error!(
                    error = %err,
                    "failed to store gossiped payload trace",
                )
            }
        });
    }

    /// Processes a gossiped cancellation message. No need to verify the signature as the message
    /// is gossiped internally and verification has been performed upstream.
    pub async fn process_gossiped_cancellation(&self, req: BroadcastCancellationParams) {
        info!(
            request_id = %req.request_id,
            "received gossiped cancellation",
        );

        let (head_slot, _) = self.curr_slot_info.read().await.clone();

        let slot = req.signed_cancellation.message.slot;

        // Verify the cancellation is for the current slot
        if slot <= head_slot {
            warn!(
                request_id = %req.request_id,
                "cancellation is for a past slot",
            );
        }

        // Verify payload has not already been delivered
        match self.auctioneer.get_last_slot_delivered().await {
            Ok(Some(del_slot)) => {
                if slot <= del_slot {
                    warn!(request_id = %req.request_id, "payload already delivered");
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!(request_id = %req.request_id, error = %err, "failed to get last slot delivered");
            }
        }

        if let Err(err) = self
            .auctioneer
            .delete_builder_bid(
                slot,
                &req.signed_cancellation.message.parent_hash,
                &req.signed_cancellation.message.proposer_public_key,
                &req.signed_cancellation.message.builder_public_key,
            )
            .await
        {
            error!(
                request_id = %req.request_id,
                error = %err,
                "Failed processing cancellable bid below floor. Could not delete builder bid.",
            );
        }
    }

    /// This function should be run as a seperate async task.
    /// Will process new gossiped messages from
    async fn process_gossiped_info(&self, mut recveiver: Receiver<GossipedMessage>) {
        while let Some(msg) = recveiver.recv().await {
            match msg {
                GossipedMessage::Header(header) => {
                    let api_clone = self.clone();
                    tokio::spawn(async move {
                        api_clone.process_gossiped_header(*header).await;
                    });
                }
                GossipedMessage::Payload(payload) => {
                    let api_clone = self.clone();
                    tokio::spawn(async move {
                        api_clone.process_gossiped_payload(*payload).await;
                    });
                }
                GossipedMessage::Cancellation(payload) => {
                    let api_clone = self.clone();
                    tokio::spawn(async move {
                        api_clone.process_gossiped_cancellation(*payload).await;
                    });
                }
                _ => {}
            }
        }
    }

    async fn gossip_new_submission(
        &self,
        payload: &SignedBidSubmission,
        execution_payload: PayloadAndBlobs,
        builder_bid: SignedBuilderBid,
        is_cancellations_enabled: bool,
        on_receive: u64,
        request_id: &Uuid,
    ) {
        self.gossip_header(
            builder_bid,
            payload.bid_trace(),
            is_cancellations_enabled,
            on_receive,
            request_id,
        )
        .await;
        self.gossip_payload(payload, execution_payload, request_id).await;
    }

    async fn gossip_header(
        &self,
        builder_bid: SignedBuilderBid,
        bid_trace: &BidTrace,
        is_cancellations_enabled: bool,
        on_receive: u64,
        request_id: &Uuid,
    ) {
        let params = BroadcastHeaderParams {
            signed_builder_bid: builder_bid,
            bid_trace: bid_trace.clone(),
            slot: bid_trace.slot,
            parent_hash: bid_trace.parent_hash.clone(),
            proposer_pub_key: bid_trace.proposer_public_key.clone(),
            builder_pub_key: bid_trace.builder_public_key.clone(),
            is_cancellations_enabled,
            on_receive,
        };
        if let Err(err) = self.gossiper.broadcast_header(params).await {
            error!(request_id = %request_id, error = %err, "failed to broadcast header");
        }
    }

    async fn gossip_payload(
        &self,
        payload: &SignedBidSubmission,
        execution_payload: PayloadAndBlobs,
        request_id: &Uuid,
    ) {
        let params = BroadcastPayloadParams {
            execution_payload,
            slot: payload.slot(),
            proposer_pub_key: payload.proposer_public_key().clone(),
        };
        if let Err(err) = self.gossiper.broadcast_payload(params).await {
            error!(request_id = %request_id, error = %err, "failed to broadcast payload");
        }
    }

    async fn gossip_cancellation(
        &self,
        signed_cancellation: SignedCancellation,
        request_id: &Uuid,
    ) {
        let params = BroadcastCancellationParams { signed_cancellation, request_id: *request_id };
        if let Err(err) = self.gossiper.broadcast_cancellation(params).await {
            error!(request_id = %request_id, error = %err, "failed to broadcast header");
        }
    }
}

// Helpers
impl<A, DB, S, G> BuilderApi<A, DB, S, G>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
    S: BlockSimulator + 'static,
    G: GossipClientTrait + 'static,
{
    /// This function verifies:
    /// 1. Runs some basic sanity checks on the payload.
    /// 2. Verifies the payload signature.
    /// 3. Simulates the submission
    ///
    /// Returns: the bid submission in an Arc.
    async fn verify_submitted_block(
        &self,
        mut payload: SignedBidSubmission,
        next_duty: BuilderGetValidatorsResponseEntry,
        builder_info: &BuilderInfo,
        trace: &mut SubmissionTrace,
        request_id: &Uuid,
        payload_attributes: &PayloadAttributesUpdate,
    ) -> Result<(Arc<SignedBidSubmission>, bool), BuilderApiError> {
        // Verify the payload signature
        if let Err(err) = payload.verify_signature(&self.chain_info.context) {
            warn!(request_id = %request_id, error = %err, "failed to verify signature");
            return Err(BuilderApiError::SignatureVerificationFailed)
        }
        trace.signature = get_nanos_timestamp()?;

        // Simulate the submission
        let payload = Arc::new(payload);
        let was_simulated_optimistically = self
            .simulate_submission(
                payload.clone(),
                builder_info,
                trace,
                next_duty.entry,
                request_id,
                payload_attributes,
            )
            .await?;

        Ok((payload, was_simulated_optimistically))
    }

    /// Check for block hashes that have already been processed.
    /// If this is the first time the hash has been seen it will insert the hash into the set.
    ///
    /// This function should not be called by functions that only process the payload.
    async fn check_for_duplicate_block_hash(
        &self,
        block_hash: &Hash32,
        slot: u64,
        parent_hash: &Hash32,
        proposer_public_key: &BlsPublicKey,
        request_id: &Uuid,
    ) -> Result<(), BuilderApiError> {
        match self
            .auctioneer
            .seen_or_insert_block_hash(block_hash, slot, parent_hash, proposer_public_key)
            .await
        {
            Ok(false) => Ok(()),
            Ok(true) => {
                warn!(request_id = %request_id, block_hash = ?block_hash, "duplicate block hash");
                Err(BuilderApiError::DuplicateBlockHash { block_hash: block_hash.clone() })
            }
            Err(err) => {
                warn!(request_id = %request_id, err = %err, "failed to call seen_or_insert_block_hash");
                Err(BuilderApiError::InternalError)
            }
        }
    }

    async fn check_tx_root_against_header(
        &self,
        payload: &SignedBidSubmission,
        request_id: &Uuid,
    ) -> Result<(), BuilderApiError> {
        match self.auctioneer.get_header_tx_root(payload.block_hash()).await {
            Ok(Some(expected_tx_root)) => {
                let tx_root = match payload.transactions_root() {
                    Some(tx_root) => tx_root,
                    None => {
                        warn!(request_id = %request_id, "no tx root found in payload");
                        return Err(BuilderApiError::MissingTransactions)
                    }
                };

                if expected_tx_root != tx_root {
                    warn!(request_id = %request_id, "tx root mismatch");
                    return Err(BuilderApiError::TransactionsRootMismatch {
                        got: Hash32::try_from(tx_root.as_ref()).unwrap(),
                        expected: Hash32::try_from(expected_tx_root.as_ref()).unwrap(),
                    })
                }
            }
            Ok(None) => {
                warn!(request_id = %request_id, "no tx root found for block hash");
                return Err(BuilderApiError::MissingTransactionsRoot)
            }
            Err(err) => {
                error!(request_id = %request_id, error = %err, "failed to get tx root");
                return Err(BuilderApiError::AuctioneerError(err))
            }
        };
        Ok(())
    }

    /// Checks if the bid in the payload is below the floor value.
    ///
    /// - If cancellations are enabled and the bid is below the floor, it deletes the previous bid.
    /// - If cancellations are not enabled and the bid is at or below the floor, then skip the
    ///   submission.
    ///
    /// Returns the floor bid value.
    async fn check_if_bid_is_below_floor(
        &self,
        slot: u64,
        parent_hash: &Hash32,
        proposer_public_key: &BlsPublicKey,
        builder_public_key: &BlsPublicKey,
        value: U256,
        is_cancellations_enabled: bool,
        request_id: &Uuid,
    ) -> Result<U256, BuilderApiError> {
        let floor_bid_value =
            match self.auctioneer.get_floor_bid_value(slot, parent_hash, proposer_public_key).await
            {
                Ok(floor_value) => floor_value.unwrap_or(U256::ZERO),
                Err(err) => {
                    error!(request_id = %request_id, error = %err, "Failed to get floor bid value");
                    return Err(BuilderApiError::InternalError)
                }
            };

        // Ignore floor bid checks if this builder pubkey is part of the
        // `skip_floor_bid_builder_pubkeys` config.
        if self.relay_config.skip_floor_bid_builder_pubkeys.contains(builder_public_key) {
            debug!(%request_id, ?builder_public_key, "skipping floor bid checks for submission");
            return Ok(floor_bid_value)
        }

        let is_bid_below_floor = value < floor_bid_value;
        let is_bid_at_or_below_floor = value <= floor_bid_value;

        if is_cancellations_enabled && is_bid_below_floor {
            debug!(request_id = %request_id, "submission below floor bid value, with cancellation");
            if let Err(err) = self
                .auctioneer
                .delete_builder_bid(slot, parent_hash, proposer_public_key, builder_public_key)
                .await
            {
                error!(
                    request_id = %request_id,
                    error = %err,
                    "Failed processing cancellable bid below floor. Could not delete builder bid.",
                );
                return Err(BuilderApiError::InternalError)
            }
            return Err(BuilderApiError::BidBelowFloor)
        } else if !is_cancellations_enabled && is_bid_at_or_below_floor {
            debug!(request_id = %request_id, "submission at or below floor bid value, without cancellation");
            return Err(BuilderApiError::BidBelowFloor)
        }
        Ok(floor_bid_value)
    }

    /// If the proposer has specified a list of trusted builders ensure
    /// that the submitting builder pubkey is in that list.
    /// Verifies that if the proposer has specified a list of trusted builders,
    /// the builder submitting a request is in that list.
    ///
    /// The auctioneer maintains a mapping of builder public keys to corresponding IDs.
    /// This function retrieves the ID associated with the builder's public key from the auctioneer.
    /// It then checks if this ID is included in the list of trusted builders specified by the
    /// proposer.
    async fn check_if_trusted_builder(
        &self,
        next_duty: &BuilderGetValidatorsResponseEntry,
        builder_info: &BuilderInfo,
    ) -> bool {
        if let Some(trusted_builders) = &next_duty.entry.preferences.trusted_builders {
            // Handle case where proposer specifies an empty list.
            if trusted_builders.is_empty() {
                return true
            }

            if let Some(builder_id) = &builder_info.builder_id {
                trusted_builders.contains(builder_id)
            } else {
                false
            }
        } else {
            true
        }
    }

    /// Simulates a new block payload.
    ///
    /// 1. Checks the current top bid value from the auctioneer.
    /// 3. Invokes the block simulator for validation.
    async fn simulate_submission(
        &self,
        payload: Arc<SignedBidSubmission>,
        builder_info: &BuilderInfo,
        trace: &mut SubmissionTrace,
        registration_info: ValidatorRegistrationInfo,
        request_id: &Uuid,
        payload_attributes: &PayloadAttributesUpdate,
    ) -> Result<bool, BuilderApiError> {
        let mut is_top_bid = false;
        match self
            .auctioneer
            .get_top_bid_value(payload.slot(), payload.parent_hash(), payload.proposer_public_key())
            .await
        {
            Ok(top_bid_value) => {
                let top_bid_value = top_bid_value.unwrap_or(U256::ZERO);
                is_top_bid = payload.value() > top_bid_value;
                info!(request_id = %request_id, top_bid_value = ?top_bid_value, new_bid_is_top_bid = is_top_bid);
            }
            Err(err) => {
                error!(request_id = %request_id, error = %err, "failed to get top bid value from auctioneer");
            }
        }

        debug!(request_id = %request_id, timestamp_before_validation = get_nanos_timestamp()?);

        let sim_request = BlockSimRequest::new(
            registration_info.registration.message.gas_limit,
            payload.clone(),
            registration_info.preferences,
            payload_attributes.payload_attributes.parent_beacon_block_root.clone(),
        );
        let result = self
            .simulator
            .process_request(
                sim_request,
                builder_info,
                is_top_bid,
                self.db_sender.clone(),
                *request_id,
            )
            .await;

        match result {
            Ok(sim_optimistic) => {
                info!(request_id = %request_id, "block simulation successful");

                trace.simulation = get_nanos_timestamp()?;
                debug!(request_id = %request_id, sim_latency = trace.simulation.saturating_sub(trace.signature));

                Ok(sim_optimistic)
            }
            Err(err) => match &err {
                BlockSimError::BlockValidationFailed(reason) => {
                    warn!(request_id = %request_id, error = %reason, "block validation failed");
                    Err(BuilderApiError::BlockValidationError(err))
                }
                _ => {
                    error!(request_id = %request_id, error = %err, "error simulating block");
                    Err(BuilderApiError::InternalError)
                }
            },
        }
    }

    async fn save_bid_to_auctioneer(
        &self,
        payload: &SignedBidSubmission,
        trace: &mut SubmissionTrace,
        is_cancellations_enabled: bool,
        floor_bid_value: U256,
        request_id: &Uuid,
    ) -> Result<Option<(SignedBuilderBid, PayloadAndBlobs)>, BuilderApiError> {
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
            Ok(Some((builder_bid, execution_payload))) => {
                // Log the results of the bid submission
                trace.auctioneer_update = get_nanos_timestamp()?;
                log_save_bid_info(
                    &update_bid_result,
                    trace.simulation,
                    trace.auctioneer_update,
                    request_id,
                );

                Ok(Some((builder_bid, execution_payload)))
            }
            Ok(None) => Ok(None),
            Err(err) => {
                error!(request_id = %request_id, error = %err, "could not save bid and update top bids");
                Err(BuilderApiError::AuctioneerError(err))
            }
        }
    }

    async fn save_header_bid_to_auctioneer(
        &self,
        payload: Arc<SignedHeaderSubmission>,
        trace: &mut HeaderSubmissionTrace,
        is_cancellations_enabled: bool,
        floor_bid_value: U256,
        request_id: &Uuid,
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
                trace.auctioneer_update = get_nanos_timestamp()?;
                log_save_bid_info(
                    &update_bid_result,
                    trace.floor_bid_checks,
                    trace.auctioneer_update,
                    request_id,
                );

                Ok(Some(builder_bid))
            }
            Ok(None) => Ok(None),
            Err(err) => {
                error!(request_id = %request_id, error = %err, "could not save header submission and update top bid");
                Err(BuilderApiError::AuctioneerError(err))
            }
        }
    }

    async fn fetch_payload_attributes(
        &self,
        slot: u64,
        parent_hash: &Hash32,
        request_id: &Uuid,
    ) -> Result<PayloadAttributesUpdate, BuilderApiError> {
        let payload_attributes_key = get_payload_attributes_key(parent_hash, slot);
        let payload_attributes =
            self.payload_attributes.read().await.get(&payload_attributes_key).cloned().ok_or_else(
                || {
                    warn!(request_id = %request_id, "payload attributes not yet known");
                    BuilderApiError::PayloadAttributesNotYetKnown
                },
            )?;

        if payload_attributes.slot != slot {
            warn!(request_id = %request_id, "payload attributes slot mismatch with payload attributes");
            return Err(BuilderApiError::PayloadSlotMismatchWithPayloadAttributes {
                got: slot,
                expected: payload_attributes.slot,
            })
        }

        Ok(payload_attributes)
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
        request_id: &Uuid,
    ) -> Result<(), BuilderApiError> {
        match self
            .auctioneer
            .get_builder_latest_payload_received_at(
                payload.slot(),
                payload.builder_public_key(),
                payload.parent_hash(),
                payload.proposer_public_key(),
            )
            .await
        {
            Ok(Some(latest_payload_received_at)) => {
                if on_receive < latest_payload_received_at {
                    return Err(BuilderApiError::AlreadyProcessingNewerPayload)
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!(request_id = %request_id, error = %err, "failed to get last slot delivered");
            }
        }
        Ok(())
    }

    /// Checks if the builder has enough collateral to submit an optimistic bid.
    /// Or if the builder is not optimistic.
    ///
    /// This function compares the builder's collateral with the block value for a bid submission.
    /// If the builder's collateral is less than the required value, it returns an error.
    async fn check_builder_collateral(
        &self,
        payload: &impl BidSubmission,
        builder_info: &BuilderInfo,
        request_id: &Uuid,
    ) -> Result<(), BuilderApiError> {
        if !builder_info.is_optimistic {
            warn!(
                request_id = %request_id,
                builder=%payload.builder_public_key(),
                "builder is not optimistic"
            );
            return Err(BuilderApiError::BuilderNotOptimistic {
                builder_pub_key: payload.builder_public_key().clone(),
            })
        } else if builder_info.collateral < payload.value() {
            warn!(
                request_id = %request_id,
                builder=?payload.builder_public_key(),
                collateral=%builder_info.collateral,
                collateral_required=%payload.value(),
                "builder does not have enough collateral"
            );
            return Err(BuilderApiError::NotEnoughOptimisticCollateral {
                builder_pub_key: payload.builder_public_key().clone(),
                collateral: builder_info.collateral,
                collateral_required: payload.value(),
                is_optimistic: builder_info.is_optimistic,
            })
        }

        // Builder has enough collateral
        Ok(())
    }

    /// Fetch the builder's information. Default info is returned if fetching fails.
    async fn fetch_builder_info(&self, builder_pub_key: &BlsPublicKey) -> BuilderInfo {
        match self.auctioneer.get_builder_info(builder_pub_key).await {
            Ok(info) => info,
            Err(err) => {
                warn!(
                    builder=?builder_pub_key,
                    err=%err,
                    "Failed to retrieve builder info"
                );
                BuilderInfo { collateral: U256::ZERO, is_optimistic: false, builder_id: None }
            }
        }
    }

    async fn demote_builder(
        &self,
        builder_pub_key: &BlsPublicKey,
        block_hash: &Hash32,
        err: &BuilderApiError,
        request_id: &Uuid,
    ) {
        error!(
            request_id = %request_id,
            error = %err,
            builder_pub_key = ?builder_pub_key,
            "verification failed for submit_block_v2. Demoting builder!",
        );

        if let Err(err) = self.auctioneer.demote_builder(builder_pub_key).await {
            error!(
                builder=%builder_pub_key,
                err=%err,
                request_id=%request_id,
                "Failed to demote builder in auctioneer"
            );
        }

        if let Err(err) =
            self.db.db_demote_builder(builder_pub_key, block_hash, err.to_string()).await
        {
            error!(
                builder=%builder_pub_key,
                err=%err,
                request_id=%request_id,
                "Failed to demote builder in database"
            );
        }
    }
}

// STATE SYNC
impl<A, DB, S, G> BuilderApi<A, DB, S, G>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
    S: BlockSimulator + 'static,
    G: GossipClientTrait + 'static,
{
    /// Subscribes to slot head updater.
    /// Updates the current slot, next proposer duty and prepares the get_validators() response.
    pub async fn housekeep(
        &self,
        slot_update_subscription: Sender<Sender<ChainUpdate>>,
    ) -> Result<(), SendError<Sender<ChainUpdate>>> {
        let (tx, mut rx) = mpsc::channel(20);
        slot_update_subscription.send(tx).await?;

        while let Some(slot_update) = rx.recv().await {
            match slot_update {
                ChainUpdate::SlotUpdate(slot_update) => {
                    self.handle_new_slot(slot_update).await;
                }
                ChainUpdate::PayloadAttributesUpdate(payload_attributes) => {
                    self.handle_new_payload_attributes(payload_attributes).await;
                }
            }
        }

        Ok(())
    }

    /// Handle a new slot update.
    /// Updates the next proposer duty and prepares the get_validators() response.
    async fn handle_new_slot(&self, slot_update: SlotUpdate) {
        let epoch = slot_update.slot / SLOTS_PER_EPOCH;
        debug!(
            epoch = epoch,
            slot_head = slot_update.slot,
            slot_start_next_epoch = (epoch + 1) * SLOTS_PER_EPOCH,
            next_proposer_duty = ?slot_update.next_duty,
            "updated head slot",
        );

        *self.curr_slot_info.write().await = (slot_update.slot, slot_update.next_duty);

        if let Some(new_duties) = slot_update.new_duties {
            let response: Vec<BuilderGetValidatorsResponse> =
                new_duties.into_iter().map(|duty| duty.into()).collect();
            match serde_json::to_vec(&response) {
                Ok(duty_bytes) => *self.proposer_duties_response.write().await = Some(duty_bytes),
                Err(err) => {
                    error!(error = %err, "failed to serialize proposer duties to JSON");
                    *self.proposer_duties_response.write().await = None;
                }
            }
        }
    }

    async fn handle_new_payload_attributes(&self, payload_attributes: PayloadAttributesUpdate) {
        let (head_slot, _) = *self.curr_slot_info.read().await;

        if payload_attributes.slot <= head_slot {
            return
        }

        debug!(
            randao = ?payload_attributes.payload_attributes.prev_randao,
            timestamp = payload_attributes.payload_attributes.timestamp,
        );

        // Discard payload attributes if already known
        let payload_attributes_key =
            get_payload_attributes_key(&payload_attributes.parent_hash, payload_attributes.slot);
        let mut all_payload_attributes = self.payload_attributes.write().await;
        if all_payload_attributes.contains_key(&payload_attributes_key) {
            return
        }

        // Clean up old payload attributes
        all_payload_attributes.retain(|_, value| value.slot >= head_slot);

        // Save new one
        all_payload_attributes.insert(payload_attributes_key, payload_attributes);
    }
}

/// `decode_payload` decodes the payload into a `SignedBidSubmission` object.
///
/// - Supports both SSZ and JSON encodings for deserialization.
/// - Automatically falls back to JSON if SSZ deserialization fails.
/// - Handles GZIP-compressed payloads.
///
/// It returns a tuple of the decoded payload and if cancellations are enabled.
pub async fn decode_payload(
    req: Request<Body>,
    trace: &mut SubmissionTrace,
    request_id: &Uuid,
) -> Result<(SignedBidSubmission, bool), BuilderApiError> {
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

    // Get content encoding and content type
    let is_gzip = req
        .headers()
        .get("Content-Encoding")
        .and_then(|val| val.to_str().ok())
        .map_or(false, |v| v == "gzip");

    let is_ssz = req
        .headers()
        .get("Content-Type")
        .and_then(|val| val.to_str().ok())
        .map_or(false, |v| v == "application/octet-stream");

    // Read the body
    let body = req.into_body();
    let mut body_bytes = to_bytes(body, MAX_PAYLOAD_LENGTH).await?;
    if body_bytes.len() > MAX_PAYLOAD_LENGTH {
        return Err(BuilderApiError::PayloadTooLarge {
            max_size: MAX_PAYLOAD_LENGTH,
            size: body_bytes.len(),
        })
    }

    // Decompress if necessary
    if is_gzip {
        let mut decoder = GzDecoder::new(&body_bytes[..]);

        // TODO: profile this. 2 is a guess.
        let estimated_size = body_bytes.len() * 2;
        let mut buf = Vec::with_capacity(estimated_size);

        decoder.read_to_end(&mut buf)?;
        body_bytes = buf.into();
    }

    // Decode payload
    let payload: SignedBidSubmission = if is_ssz {
        match ssz::prelude::deserialize(&body_bytes) {
            Ok(payload) => payload,
            Err(err) => {
                // Fallback to JSON
                warn!(request_id = %request_id, error = %err, "Failed to decode payload using SSZ; falling back to JSON");
                serde_json::from_slice(&body_bytes)?
            }
        }
    } else {
        serde_json::from_slice(&body_bytes)?
    };

    trace.decode = get_nanos_timestamp()?;
    info!(
        request_id = %request_id,
        timestamp_after_decoding = trace.decode,
        decode_latency_ns = trace.decode.saturating_sub(trace.receive),
        builder_pub_key = ?payload.builder_public_key(),
        block_hash = ?payload.block_hash(),
        proposer_pubkey = ?payload.proposer_public_key(),
        parent_hash = ?payload.parent_hash(),
        value = ?payload.value(),
        num_tx = payload.execution_payload().transactions().len(),
    );

    Ok((payload, is_cancellations_enabled))
}

/// `push_top_bids` manages a WebSocket connection to continuously send the top auction bids to a
/// client.
///
/// - Periodically fetches the latest auction bids via a stream and sends them to the client in ssz
///   format.
/// - Sends a ping message every 10 seconds to maintain the connection's liveliness.
/// - Terminates the connection on sending failures or if a bid stream error occurs, ensuring clean
///   disconnection.
///
/// This function operates in an asynchronous loop until the WebSocket connection is closed either
/// due to an error or when the auction ends. It returns after the socket has been closed, logging
/// the closure status.
async fn push_top_bids<A: Auctioneer + 'static>(mut socket: WebSocket, auctioneer: Arc<A>) {
    let mut bid_stream = auctioneer.get_best_bids().await;
    let mut interval = time::interval(Duration::from_secs(10));

    loop {
        tokio::select! {
            Some(result) = bid_stream.next() => {
                match result {
                    Ok(bid) => {
                        if socket.send(Message::Binary(bid)).await.is_err() {
                            error!("Failed to send bid. Disconnecting.");
                            break;
                        }
                    },
                    Err(e) => {
                        error!("Error while receiving bid: {}", e);
                        break;
                    }
                }
            },
            _ = interval.tick() => {
                if socket.send(Message::Ping(Vec::new())).await.is_err() {
                    error!("Failed to send ping.");
                    break;
                }
            },
            msg = socket.next() => {
                match msg {
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            error!("Failed to respond to ping.");
                            break;
                        }
                    },
                    Some(Ok(Message::Pong(_))) => {
                        debug!("Received pong response.");
                    },
                    Some(Ok(Message::Close(_))) => {
                        debug!("Received close frame.");
                        break;
                    },
                    Some(Ok(Message::Binary(_))) => {
                        debug!("Received Binary frame.");
                    },
                    Some(Ok(Message::Text(_))) => {
                        debug!("Received Text frame.");
                    },
                    Some(Err(e)) => {
                        error!("Error in WebSocket connection: {}", e);
                        break;
                    },
                    None => {
                        error!("WebSocket connection closed by the other side.");
                        break;
                    }
                }
            }
        }
    }
    if let Err(e) = socket.close().await {
        error!("Failed to close socket properly: {}", e);
    }
    debug!("Socket connection closed gracefully.");
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
    request_id: &Uuid,
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

    let is_ssz = req
        .headers()
        .get("Content-Type")
        .and_then(|val| val.to_str().ok())
        .map_or(false, |v| v == "application/octet-stream");

    // Read the body
    let body = req.into_body();
    let body_bytes = to_bytes(body, MAX_PAYLOAD_LENGTH).await?;
    if body_bytes.len() > MAX_PAYLOAD_LENGTH {
        return Err(BuilderApiError::PayloadTooLarge {
            max_size: MAX_PAYLOAD_LENGTH,
            size: body_bytes.len(),
        })
    }

    // Decode header
    let header: SignedHeaderSubmission = if is_ssz {
        match ssz::prelude::deserialize(&body_bytes) {
            Ok(header) => header,
            Err(err) => {
                // Fallback to JSON
                warn!(request_id = %request_id, error = %err, "Failed to decode header using SSZ; falling back to JSON");
                serde_json::from_slice(&body_bytes)?
            }
        }
    } else {
        serde_json::from_slice(&body_bytes)?
    };

    trace.decode = get_nanos_timestamp()?;
    info!(
        request_id = %request_id,
        timestamp_after_decoding = Instant::now().elapsed().as_nanos(),
        decode_latency_ns = trace.decode.saturating_sub(trace.receive),
        builder_pub_key = ?header.builder_public_key(),
        block_hash = ?header.block_hash(),
        proposer_pubkey = ?header.proposer_public_key(),
        parent_hash = ?header.parent_hash(),
        value = ?header.value(),
    );

    Ok((header, is_cancellations_enabled))
}

/// - Validates the expected block.timestamp.
/// - Ensures that the fee recipients in the payload and proposer duty match.
/// - Ensures that the slot in the payload and payload attributes match.
/// - Validates that the block hash in the payload and message are the same.
/// - Validates that the parent hash in the payload and message are the same.
fn sanity_check_block_submission(
    payload: &impl BidSubmission,
    bid_trace: &BidTrace,
    next_duty: &BuilderGetValidatorsResponseEntry,
    payload_attributes: &PayloadAttributesUpdate,
    chain_info: &ChainInfo,
) -> Result<(), BuilderApiError> {
    let expected_timestamp = chain_info.genesis_time_in_secs + (bid_trace.slot * SECONDS_PER_SLOT);
    if payload.timestamp() != expected_timestamp {
        return Err(BuilderApiError::IncorrectTimestamp {
            got: payload.timestamp(),
            expected: expected_timestamp,
        })
    }

    // Check duty
    if next_duty.entry.registration.message.fee_recipient != *payload.proposer_fee_recipient() {
        return Err(BuilderApiError::FeeRecipientMismatch {
            got: payload.proposer_fee_recipient().clone(),
            expected: next_duty.entry.registration.message.fee_recipient.clone(),
        })
    }

    if payload.slot() != next_duty.slot {
        return Err(BuilderApiError::SlotMismatch { got: payload.slot(), expected: next_duty.slot })
    }

    if next_duty.entry.registration.message.public_key != bid_trace.proposer_public_key {
        return Err(BuilderApiError::ProposerPublicKeyMismatch {
            got: bid_trace.proposer_public_key.clone(),
            expected: next_duty.entry.registration.message.public_key.clone(),
        })
    }

    // Check payload attrs
    if *payload.prev_randao() != payload_attributes.payload_attributes.prev_randao {
        return Err(BuilderApiError::PrevRandaoMismatch {
            got: payload.prev_randao().clone(),
            expected: payload_attributes.payload_attributes.prev_randao.clone(),
        })
    }

    if has_reached_fork(payload.slot(), CAPELLA_FORK_EPOCH) {
        if payload.is_full_payload() {
            let withdrawals_root = match payload.withdrawals_root() {
                Some(w) => w,
                None => return Err(BuilderApiError::MissingWithdrawls),
            };

            let expected_withdrawals_root = match payload_attributes.withdrawals_root {
                Some(wr) => wr,
                None => return Err(BuilderApiError::MissingWithdrawls),
            };

            if withdrawals_root != expected_withdrawals_root {
                return Err(BuilderApiError::WithdrawalsRootMismatch {
                    got: Hash32::try_from(withdrawals_root.as_ref()).unwrap(),
                    expected: Hash32::try_from(expected_withdrawals_root.as_ref()).unwrap(),
                })
            }
        } else {
            let expected_withdrawals_root = match payload_attributes.withdrawals_root {
                Some(wr) => wr,
                None => return Err(BuilderApiError::MissingWithdrawlsRoot),
            };

            let payload_withdrawals_root = match payload.withdrawals_root() {
                Some(wr) => wr,
                None => return Err(BuilderApiError::MissingWithdrawlsRoot),
            };

            if payload_withdrawals_root != expected_withdrawals_root {
                return Err(BuilderApiError::WithdrawalsRootMismatch {
                    got: Hash32::try_from(payload_withdrawals_root.as_ref()).unwrap(),
                    expected: Hash32::try_from(expected_withdrawals_root.as_ref()).unwrap(),
                })
            }
        }
    }

    // Misc. sanity checks
    if payload.value() == U256::ZERO {
        return Err(BuilderApiError::ZeroValueBlock)
    }

    if bid_trace.block_hash != *payload.block_hash() {
        return Err(BuilderApiError::BlockHashMismatch {
            message: bid_trace.block_hash.clone(),
            payload: payload.block_hash().clone(),
        })
    }

    if bid_trace.parent_hash != *payload.parent_hash() {
        return Err(BuilderApiError::ParentHashMismatch {
            message: bid_trace.parent_hash.clone(),
            payload: payload.parent_hash().clone(),
        })
    }

    Ok(())
}

fn log_save_bid_info(
    update_bid_result: &SaveBidAndUpdateTopBidResponse,
    bid_update_start: u64,
    bid_update_finish: u64,
    request_id: &Uuid,
) {
    info!(
        request_id = %request_id,
        bid_update_latency = bid_update_finish.saturating_sub(bid_update_start),
        was_bid_saved_in = update_bid_result.was_bid_saved,
        was_top_bid_updated = update_bid_result.was_top_bid_updated,
        top_bid_value = ?update_bid_result.top_bid_value,
        prev_top_bid_value = ?update_bid_result.prev_top_bid_value,
        save_payload = update_bid_result.latency_save_payload,
        update_top_bid = update_bid_result.latency_update_top_bid,
        update_floor = update_bid_result.latency_update_floor,
    );

    if update_bid_result.was_bid_saved {
        debug!(request_id = %request_id, eligible_at = bid_update_finish);
    }
}

/// Should be called as a new async task.
/// Stores updates to the db out of the critical path.
async fn process_db_additions<DB: DatabaseService + 'static>(
    db: Arc<DB>,
    mut db_receiver: mpsc::Receiver<DbInfo>,
) {
    while let Some(db_info) = db_receiver.recv().await {
        match db_info {
            DbInfo::NewSubmission(submission, trace, version) => {
                if let Err(err) = db.store_block_submission(submission, trace, version as i16).await
                {
                    error!(
                        error = %err,
                        "failed to store block submission",
                    )
                }
            }
            DbInfo::NewHeaderSubmission(header_submission, trace) => {
                if let Err(err) = db.store_header_submission(header_submission, trace).await {
                    error!(
                        error = %err,
                        "failed to store header submission",
                    )
                }
            }
            DbInfo::GossipedHeader { block_hash, trace } => {
                if let Err(err) = db.save_gossiped_header_trace(block_hash, trace).await {
                    error!(
                        error = %err,
                        "failed to store gossiped header trace",
                    )
                }
            }
            DbInfo::GossipedPayload { block_hash, trace } => {
                if let Err(err) = db.save_gossiped_payload_trace(block_hash, trace).await {
                    error!(
                        error = %err,
                        "failed to store gossiped payload trace",
                    )
                }
            }
            DbInfo::SimulationResult { block_hash, block_sim_result } => {
                if let Err(err) = db.save_simulation_result(block_hash, block_sim_result).await {
                    error!(
                        error = %err,
                        "failed to store simulation result",
                    )
                }
            }
        }
    }
}

fn get_nanos_timestamp() -> Result<u64, BuilderApiError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .map_err(|_| BuilderApiError::InternalError)
}

fn get_nanos_from(now: SystemTime) -> Result<u64, BuilderApiError> {
    now.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .map_err(|_| BuilderApiError::InternalError)
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::http::{
        header::{CONTENT_ENCODING, CONTENT_TYPE},
        HeaderValue, Uri,
    };

    use uuid::Uuid;

    async fn build_test_request(payload: Vec<u8>, is_gzip: bool, is_ssz: bool) -> Request<Body> {
        let mut req = Request::new(Body::from(payload));
        *req.uri_mut() = Uri::from_static("/some_path?cancellations=1");

        if is_gzip {
            req.headers_mut().insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        }

        if is_ssz {
            req.headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
        }

        req
    }

    async fn create_test_submission_trace() -> SubmissionTrace {
        SubmissionTrace::default()
    }

    async fn create_test_uuid() -> Uuid {
        Uuid::new_v4()
    }

    #[tokio::test]
    async fn test_decode_json_payload() {
        let json_payload: Vec<u8> = vec![];

        match serde_json::from_slice::<SignedBidSubmission>(&json_payload) {
            Ok(res) => {
                println!("THIS IS THE RESULT: {:?}", res);
            }
            Err(err) => {
                println!("THIS IS THE ERR: {:?}", err);
            }
        }
    }

    #[tokio::test]
    async fn test_decode_empty_tx_payload_json() {
        let json_payload = vec![
            123, 10, 32, 32, 34, 109, 101, 115, 115, 97, 103, 101, 34, 58, 32, 123, 10, 32, 32, 32,
            32, 34, 115, 108, 111, 116, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34, 112,
            97, 114, 101, 110, 116, 95, 104, 97, 115, 104, 34, 58, 32, 34, 48, 120, 99, 102, 56,
            101, 48, 100, 52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48, 49, 100, 48, 55, 57,
            48, 51, 52, 55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 52, 51, 100, 53, 97, 49, 56,
            56, 52, 53, 54, 48, 51, 54, 55, 101, 56, 50, 48, 56, 100, 57, 50, 48, 102, 50, 34, 44,
            10, 32, 32, 32, 32, 34, 98, 108, 111, 99, 107, 95, 104, 97, 115, 104, 34, 58, 32, 34,
            48, 120, 99, 102, 56, 101, 48, 100, 52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51,
            48, 49, 100, 48, 55, 57, 48, 51, 52, 55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 52,
            51, 100, 53, 97, 49, 56, 56, 52, 53, 54, 48, 51, 54, 55, 101, 56, 50, 48, 56, 100, 57,
            50, 48, 102, 50, 34, 44, 10, 32, 32, 32, 32, 34, 98, 117, 105, 108, 100, 101, 114, 95,
            112, 117, 98, 107, 101, 121, 34, 58, 32, 34, 48, 120, 57, 51, 50, 52, 55, 102, 50, 50,
            48, 57, 97, 98, 99, 97, 99, 102, 53, 55, 98, 55, 53, 97, 53, 49, 100, 97, 102, 97, 101,
            55, 55, 55, 102, 57, 100, 100, 51, 56, 98, 99, 55, 48, 53, 51, 100, 49, 97, 102, 53,
            50, 54, 102, 50, 50, 48, 97, 55, 52, 56, 57, 97, 54, 100, 51, 97, 50, 55, 53, 51, 101,
            53, 102, 51, 101, 56, 98, 49, 99, 102, 101, 51, 57, 98, 53, 54, 102, 52, 51, 54, 49,
            49, 100, 102, 55, 52, 97, 34, 44, 10, 32, 32, 32, 32, 34, 112, 114, 111, 112, 111, 115,
            101, 114, 95, 112, 117, 98, 107, 101, 121, 34, 58, 32, 34, 48, 120, 56, 53, 53, 57, 55,
            50, 55, 101, 101, 54, 53, 99, 50, 57, 53, 50, 55, 57, 51, 51, 50, 49, 57, 56, 48, 50,
            57, 99, 57, 51, 57, 53, 53, 55, 102, 52, 100, 50, 97, 98, 97, 48, 55, 53, 49, 102, 99,
            53, 53, 102, 55, 49, 100, 48, 55, 51, 51, 98, 56, 97, 97, 49, 55, 99, 100, 48, 51, 48,
            49, 50, 51, 50, 97, 55, 102, 50, 49, 97, 56, 57, 53, 102, 56, 49, 101, 97, 99, 102, 53,
            53, 99, 57, 55, 101, 99, 52, 34, 44, 10, 32, 32, 32, 32, 34, 112, 114, 111, 112, 111,
            115, 101, 114, 95, 102, 101, 101, 95, 114, 101, 99, 105, 112, 105, 101, 110, 116, 34,
            58, 32, 34, 48, 120, 97, 98, 99, 102, 56, 101, 48, 100, 52, 101, 57, 53, 56, 55, 51,
            54, 57, 98, 50, 51, 48, 49, 100, 48, 55, 57, 48, 51, 52, 55, 51, 50, 48, 51, 48, 50,
            99, 99, 48, 57, 34, 44, 10, 32, 32, 32, 32, 34, 103, 97, 115, 95, 108, 105, 109, 105,
            116, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34, 103, 97, 115, 95, 117, 115,
            101, 100, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34, 118, 97, 108, 117, 101,
            34, 58, 32, 34, 49, 34, 10, 32, 32, 125, 44, 10, 32, 32, 34, 101, 120, 101, 99, 117,
            116, 105, 111, 110, 95, 112, 97, 121, 108, 111, 97, 100, 34, 58, 32, 123, 10, 32, 32,
            32, 32, 34, 112, 97, 114, 101, 110, 116, 95, 104, 97, 115, 104, 34, 58, 32, 34, 48,
            120, 99, 102, 56, 101, 48, 100, 52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48,
            49, 100, 48, 55, 57, 48, 51, 52, 55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 52, 51,
            100, 53, 97, 49, 56, 56, 52, 53, 54, 48, 51, 54, 55, 101, 56, 50, 48, 56, 100, 57, 50,
            48, 102, 50, 34, 44, 10, 32, 32, 32, 32, 34, 102, 101, 101, 95, 114, 101, 99, 105, 112,
            105, 101, 110, 116, 34, 58, 32, 34, 48, 120, 97, 98, 99, 102, 56, 101, 48, 100, 52,
            101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48, 49, 100, 48, 55, 57, 48, 51, 52, 55,
            51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 34, 44, 10, 32, 32, 32, 32, 34, 115, 116, 97,
            116, 101, 95, 114, 111, 111, 116, 34, 58, 32, 34, 48, 120, 99, 102, 56, 101, 48, 100,
            52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48, 49, 100, 48, 55, 57, 48, 51, 52,
            55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 52, 51, 100, 53, 97, 49, 56, 56, 52, 53,
            54, 48, 51, 54, 55, 101, 56, 50, 48, 56, 100, 57, 50, 48, 102, 50, 34, 44, 10, 32, 32,
            32, 32, 34, 114, 101, 99, 101, 105, 112, 116, 115, 95, 114, 111, 111, 116, 34, 58, 32,
            34, 48, 120, 99, 102, 56, 101, 48, 100, 52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50,
            51, 48, 49, 100, 48, 55, 57, 48, 51, 52, 55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57,
            52, 51, 100, 53, 97, 49, 56, 56, 52, 53, 54, 48, 51, 54, 55, 101, 56, 50, 48, 56, 100,
            57, 50, 48, 102, 50, 34, 44, 10, 32, 32, 32, 32, 34, 108, 111, 103, 115, 95, 98, 108,
            111, 111, 109, 34, 58, 32, 34, 48, 120, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 10, 32, 32, 32,
            32, 34, 112, 114, 101, 118, 95, 114, 97, 110, 100, 97, 111, 34, 58, 32, 34, 48, 120,
            99, 102, 56, 101, 48, 100, 52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48, 49,
            100, 48, 55, 57, 48, 51, 52, 55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 52, 51, 100,
            53, 97, 49, 56, 56, 52, 53, 54, 48, 51, 54, 55, 101, 56, 50, 48, 56, 100, 57, 50, 48,
            102, 50, 34, 44, 10, 32, 32, 32, 32, 34, 98, 108, 111, 99, 107, 95, 110, 117, 109, 98,
            101, 114, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34, 103, 97, 115, 95, 108,
            105, 109, 105, 116, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34, 103, 97, 115,
            95, 117, 115, 101, 100, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34, 116, 105,
            109, 101, 115, 116, 97, 109, 112, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34,
            101, 120, 116, 114, 97, 95, 100, 97, 116, 97, 34, 58, 32, 34, 48, 120, 99, 102, 56,
            101, 48, 100, 52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48, 49, 100, 48, 55, 57,
            48, 51, 52, 55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 52, 51, 100, 53, 97, 49, 56,
            56, 52, 53, 54, 48, 51, 54, 55, 101, 56, 50, 48, 56, 100, 57, 50, 48, 102, 50, 34, 44,
            10, 32, 32, 32, 32, 34, 98, 97, 115, 101, 95, 102, 101, 101, 95, 112, 101, 114, 95,
            103, 97, 115, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34, 98, 108, 111, 99,
            107, 95, 104, 97, 115, 104, 34, 58, 32, 34, 48, 120, 99, 102, 56, 101, 48, 100, 52,
            101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48, 49, 100, 48, 55, 57, 48, 51, 52, 55,
            51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 52, 51, 100, 53, 97, 49, 56, 56, 52, 53, 54,
            48, 51, 54, 55, 101, 56, 50, 48, 56, 100, 57, 50, 48, 102, 50, 34, 44, 10, 32, 32, 32,
            32, 34, 116, 114, 97, 110, 115, 97, 99, 116, 105, 111, 110, 115, 34, 58, 32, 91, 10,
            32, 32, 32, 32, 32, 32, 34, 48, 120, 48, 50, 102, 56, 55, 56, 56, 51, 49, 52, 54, 57,
            54, 54, 56, 51, 48, 51, 102, 53, 49, 100, 56, 52, 51, 98, 57, 97, 99, 57, 102, 57, 56,
            52, 51, 98, 57, 97, 99, 97, 48, 48, 56, 50, 53, 50, 48, 56, 57, 52, 99, 57, 51, 50, 54,
            57, 98, 55, 51, 48, 57, 54, 57, 57, 56, 100, 98, 54, 54, 98, 101, 48, 52, 52, 49, 101,
            56, 51, 54, 100, 56, 55, 51, 53, 51, 53, 99, 98, 57, 99, 56, 56, 57, 52, 97, 49, 57,
            48, 52, 49, 56, 56, 54, 102, 48, 48, 48, 48, 56, 48, 99, 48, 48, 49, 97, 48, 51, 49,
            99, 99, 50, 57, 50, 51, 52, 48, 51, 54, 97, 102, 98, 102, 57, 97, 49, 102, 98, 57, 52,
            55, 54, 98, 52, 54, 51, 51, 54, 55, 99, 98, 49, 102, 57, 53, 55, 97, 99, 48, 98, 57,
            49, 57, 98, 54, 57, 98, 98, 99, 55, 57, 56, 52, 51, 54, 101, 54, 48, 52, 97, 97, 97,
            48, 49, 56, 99, 52, 101, 57, 99, 51, 57, 49, 52, 101, 98, 50, 55, 97, 97, 100, 100, 48,
            98, 57, 49, 101, 49, 48, 98, 49, 56, 54, 53, 53, 55, 51, 57, 102, 99, 102, 56, 99, 49,
            102, 99, 51, 57, 56, 55, 54, 51, 97, 57, 102, 49, 98, 101, 101, 99, 98, 56, 100, 100,
            99, 56, 54, 34, 10, 32, 32, 32, 32, 93, 44, 10, 32, 32, 32, 32, 34, 119, 105, 116, 104,
            100, 114, 97, 119, 97, 108, 115, 34, 58, 32, 91, 10, 32, 32, 32, 32, 32, 32, 123, 10,
            32, 32, 32, 32, 32, 32, 32, 32, 34, 105, 110, 100, 101, 120, 34, 58, 32, 34, 49, 34,
            44, 10, 32, 32, 32, 32, 32, 32, 32, 32, 34, 118, 97, 108, 105, 100, 97, 116, 111, 114,
            95, 105, 110, 100, 101, 120, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 32, 32,
            32, 32, 34, 97, 100, 100, 114, 101, 115, 115, 34, 58, 32, 34, 48, 120, 97, 98, 99, 102,
            56, 101, 48, 100, 52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48, 49, 100, 48, 55,
            57, 48, 51, 52, 55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 34, 44, 10, 32, 32, 32, 32,
            32, 32, 32, 32, 34, 97, 109, 111, 117, 110, 116, 34, 58, 32, 34, 51, 50, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 34, 10, 32, 32, 32, 32, 32, 32, 125, 10, 32, 32, 32, 32, 93,
            10, 32, 32, 125, 44, 10, 32, 32, 34, 115, 105, 103, 110, 97, 116, 117, 114, 101, 34,
            58, 32, 34, 48, 120, 49, 98, 54, 54, 97, 99, 49, 102, 98, 54, 54, 51, 99, 57, 98, 99,
            53, 57, 53, 48, 57, 56, 52, 54, 100, 54, 101, 99, 48, 53, 51, 52, 53, 98, 100, 57, 48,
            56, 101, 100, 97, 55, 51, 101, 54, 55, 48, 97, 102, 56, 56, 56, 100, 97, 52, 49, 97,
            102, 49, 55, 49, 53, 48, 53, 99, 99, 52, 49, 49, 100, 54, 49, 50, 53, 50, 102, 98, 54,
            99, 98, 51, 102, 97, 48, 48, 49, 55, 98, 54, 55, 57, 102, 56, 98, 98, 50, 51, 48, 53,
            98, 50, 54, 97, 50, 56, 53, 102, 97, 50, 55, 51, 55, 102, 49, 55, 53, 54, 54, 56, 100,
            48, 100, 102, 102, 57, 49, 99, 99, 49, 98, 54, 54, 97, 99, 49, 102, 98, 54, 54, 51, 99,
            57, 98, 99, 53, 57, 53, 48, 57, 56, 52, 54, 100, 54, 101, 99, 48, 53, 51, 52, 53, 98,
            100, 57, 48, 56, 101, 100, 97, 55, 51, 101, 54, 55, 48, 97, 102, 56, 56, 56, 100, 97,
            52, 49, 97, 102, 49, 55, 49, 53, 48, 53, 34, 10, 125,
        ];
        match serde_json::from_slice::<SignedBidSubmission>(&json_payload) {
            Ok(res) => {
                println!("THIS IS THE RESULT: {:?}", res);
            }
            Err(err) => {
                println!("THIS IS THE ERR: {:?}", err);
            }
        }
    }

    #[tokio::test]
    async fn test_decode_payload_ssz() {
        let ssz_payload: Vec<u8> = vec![];
        match ssz::prelude::deserialize::<SignedBidSubmission>(&ssz_payload) {
            Ok(res) => {
                println!("THIS IS THE RESULT: {:?}", res);
            }
            Err(err) => {
                println!("THIS IS THE ERR: {:?}", err);
            }
        }
    }

    #[tokio::test]
    async fn test_decode_ssz_payload_empty() {
        let ssz_payload = vec![
            178, 184, 84, 0, 0, 0, 0, 0, 189, 50, 145, 133, 77, 200, 34, 183, 236, 88, 89, 37, 205,
            160, 225, 143, 6, 175, 40, 250, 40, 134, 225, 95, 82, 213, 45, 212, 182, 249, 78, 214,
            27, 175, 220, 69, 65, 22, 182, 5, 0, 83, 100, 151, 107, 19, 77, 118, 29, 215, 54, 203,
            71, 136, 210, 92, 131, 87, 131, 180, 109, 174, 177, 33, 182, 122, 81, 72, 160, 50, 41,
            146, 110, 52, 177, 144, 175, 129, 168, 42, 129, 196, 223, 102, 131, 28, 152, 192, 58,
            19, 151, 120, 65, 141, 208, 154, 59, 84, 44, 237, 0, 34, 98, 13, 25, 243, 87, 129, 236,
            230, 220, 54, 133, 89, 114, 126, 230, 92, 41, 82, 121, 51, 33, 152, 2, 156, 147, 149,
            87, 244, 210, 171, 160, 117, 31, 197, 95, 113, 208, 115, 59, 138, 161, 124, 208, 48,
            18, 50, 167, 242, 26, 137, 95, 129, 234, 207, 85, 201, 126, 196, 92, 192, 221, 225, 78,
            114, 86, 52, 12, 200, 32, 65, 90, 96, 34, 167, 209, 201, 58, 53, 128, 195, 201, 1, 0,
            0, 0, 0, 205, 212, 138, 1, 0, 0, 0, 0, 103, 160, 177, 121, 204, 223, 252, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 80, 1, 0, 0, 162, 222,
            245, 66, 55, 191, 235, 29, 146, 105, 54, 94, 133, 59, 84, 105, 246, 139, 127, 74, 213,
            28, 167, 135, 126, 64, 108, 169, 75, 200, 169, 75, 186, 84, 193, 64, 36, 178, 249, 237,
            55, 216, 105, 11, 185, 250, 197, 38, 0, 183, 255, 82, 185, 107, 132, 60, 216, 82, 158,
            158, 204, 36, 151, 160, 236, 213, 219, 131, 114, 226, 4, 145, 86, 224, 250, 147, 52,
            213, 193, 176, 239, 100, 47, 25, 38, 117, 181, 134, 236, 190, 111, 195, 129, 23, 143,
            136, 189, 50, 145, 133, 77, 200, 34, 183, 236, 88, 89, 37, 205, 160, 225, 143, 6, 175,
            40, 250, 40, 134, 225, 95, 82, 213, 45, 212, 182, 249, 78, 214, 182, 74, 48, 57, 159,
            127, 107, 12, 21, 76, 46, 122, 240, 163, 236, 123, 10, 91, 19, 26, 116, 247, 77, 21,
            220, 176, 11, 161, 148, 144, 17, 54, 242, 1, 157, 214, 190, 45, 76, 136, 200, 34, 120,
            109, 249, 5, 97, 165, 80, 25, 56, 153, 180, 16, 250, 52, 161, 49, 38, 38, 219, 60, 65,
            211, 37, 84, 94, 159, 36, 233, 3, 213, 221, 155, 156, 38, 8, 130, 170, 219, 76, 40,
            250, 142, 64, 247, 72, 69, 33, 226, 244, 248, 0, 30, 217, 176, 129, 5, 149, 196, 80,
            56, 8, 68, 175, 97, 9, 68, 33, 129, 4, 210, 28, 34, 0, 40, 170, 106, 21, 10, 134, 49,
            43, 0, 0, 64, 64, 25, 9, 229, 72, 231, 28, 106, 5, 18, 138, 88, 96, 114, 0, 16, 63,
            196, 0, 52, 111, 110, 152, 67, 194, 226, 12, 114, 160, 13, 123, 216, 19, 138, 36, 154,
            131, 32, 134, 151, 192, 208, 61, 118, 51, 193, 130, 72, 165, 143, 232, 130, 132, 36,
            10, 14, 67, 231, 130, 198, 176, 193, 22, 122, 0, 172, 152, 8, 190, 73, 153, 80, 232,
            80, 2, 40, 119, 105, 128, 38, 100, 144, 242, 28, 144, 64, 179, 10, 48, 180, 146, 44,
            58, 122, 248, 80, 186, 8, 242, 0, 1, 70, 234, 133, 4, 1, 233, 120, 3, 142, 18, 5, 9,
            66, 21, 2, 80, 22, 174, 136, 17, 22, 105, 97, 71, 125, 104, 82, 44, 130, 108, 154, 13,
            32, 34, 140, 130, 45, 226, 172, 131, 5, 3, 177, 57, 54, 181, 224, 27, 159, 149, 50,
            236, 35, 34, 199, 12, 73, 28, 26, 33, 97, 133, 51, 194, 132, 41, 155, 24, 146, 7, 207,
            14, 55, 242, 199, 161, 147, 12, 102, 103, 129, 95, 210, 56, 41, 9, 38, 38, 92, 194,
            128, 149, 160, 160, 36, 2, 52, 175, 56, 16, 146, 138, 150, 42, 208, 38, 74, 73, 5, 1,
            138, 2, 161, 153, 98, 129, 110, 157, 10, 57, 253, 76, 128, 147, 83, 56, 167, 65, 220,
            145, 109, 21, 69, 105, 78, 65, 235, 90, 80, 94, 26, 48, 152, 249, 228, 220, 89, 136, 0,
            0, 0, 0, 0, 128, 195, 201, 1, 0, 0, 0, 0, 205, 212, 138, 1, 0, 0, 0, 0, 184, 156, 82,
            100, 0, 0, 0, 0, 0, 2, 0, 0, 255, 18, 249, 112, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 27, 175, 220, 69, 65, 22, 182, 5, 0, 83,
            100, 151, 107, 19, 77, 118, 29, 215, 54, 203, 71, 136, 210, 92, 131, 87, 131, 180, 109,
            174, 177, 33, 31, 2, 0, 0, 31, 2, 0, 0, 73, 108, 108, 117, 109, 105, 110, 97, 116, 101,
            32, 68, 109, 111, 99, 114, 97, 116, 105, 122, 101, 32, 68, 115, 116, 114, 105, 98, 117,
            116, 101, 75, 38, 68, 0, 0, 0, 0, 0, 84, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 86, 104, 29, 0, 0,
            0, 0, 0, 76, 38, 68, 0, 0, 0, 0, 0, 85, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 161, 141, 29, 0, 0,
            0, 0, 0, 77, 38, 68, 0, 0, 0, 0, 0, 86, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 212, 184, 28, 0, 0,
            0, 0, 0, 78, 38, 68, 0, 0, 0, 0, 0, 87, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 157, 132, 29, 0, 0,
            0, 0, 0, 79, 38, 68, 0, 0, 0, 0, 0, 88, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 52, 170, 29, 0, 0,
            0, 0, 0, 80, 38, 68, 0, 0, 0, 0, 0, 89, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 121, 155, 29, 0, 0,
            0, 0, 0, 81, 38, 68, 0, 0, 0, 0, 0, 90, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 225, 68, 29, 0, 0,
            0, 0, 0, 82, 38, 68, 0, 0, 0, 0, 0, 91, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 39, 49, 29, 0, 0, 0,
            0, 0, 83, 38, 68, 0, 0, 0, 0, 0, 92, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218,
            1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 209, 107, 28, 0, 0, 0, 0,
            0, 84, 38, 68, 0, 0, 0, 0, 0, 93, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218, 1,
            5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 110, 85, 29, 0, 0, 0, 0, 0,
            85, 38, 68, 0, 0, 0, 0, 0, 94, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218, 1, 5,
            124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 14, 157, 29, 0, 0, 0, 0, 0, 86,
            38, 68, 0, 0, 0, 0, 0, 95, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218, 1, 5, 124,
            8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 41, 72, 29, 0, 0, 0, 0, 0, 87, 38,
            68, 0, 0, 0, 0, 0, 96, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218, 1, 5, 124, 8,
            228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 105, 5, 29, 0, 0, 0, 0, 0, 88, 38, 68,
            0, 0, 0, 0, 0, 97, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218, 1, 5, 124, 8, 228,
            193, 186, 168, 250, 166, 41, 129, 156, 42, 90, 141, 28, 0, 0, 0, 0, 0, 89, 38, 68, 0,
            0, 0, 0, 0, 98, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218, 1, 5, 124, 8, 228,
            193, 186, 168, 250, 166, 41, 129, 156, 42, 170, 78, 28, 0, 0, 0, 0, 0, 90, 38, 68, 0,
            0, 0, 0, 0, 99, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218, 1, 5, 124, 8, 228,
            193, 186, 168, 250, 166, 41, 129, 156, 42, 209, 28, 29, 0, 0, 0, 0, 0,
        ];

        match ssz::prelude::deserialize::<SignedBidSubmission>(&ssz_payload) {
            Ok(res) => {
                println!("THIS IS THE RESULT: {:?}", res);
            }
            Err(err) => {
                println!("THIS IS THE ERR: {:?}", err);
            }
        }
    }

    #[tokio::test]
    async fn test_decode_payload_too_large() {
        let payload = vec![0u8; MAX_PAYLOAD_LENGTH + 1];
        let req = build_test_request(payload, false, false).await;
        let mut trace = create_test_submission_trace().await;
        let request_id = create_test_uuid().await;

        let result = decode_payload(req, &mut trace, &request_id).await;
        match result {
            Ok(_) => panic!("Should have failed"),
            Err(err) => match err {
                BuilderApiError::AxumError(err) => {
                    assert_eq!(err.to_string(), "length limit exceeded");
                }
                _ => panic!("Should have failed with AxumError"),
            },
        }
    }
}
