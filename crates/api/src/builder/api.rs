use std::{
    collections::HashMap,
    collections::HashSet,
    io::Read,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension,
};
use ethereum_consensus::{
    configs::{mainnet::CAPELLA_FORK_EPOCH, mainnet::SECONDS_PER_SLOT},
    phase0::mainnet::SLOTS_PER_EPOCH,
    primitives::{Bytes32, Hash32},
    ssz::{self, prelude::*},
};
use flate2::read::GzDecoder;
use hyper::{Body, Request};
use tokio::{
    sync::{
        mpsc::{self, error::SendError, Sender},
        RwLock,
    },
    time::Instant,
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use helix_database::DatabaseService;
use helix_datastore::types::SaveBidAndUpdateTopBidResponse;
use helix_datastore::Auctioneer;
use helix_housekeeper::{ChainUpdate, PayloadAttributesUpdate, SlotUpdate};
use helix_common::api::builder_api::BuilderGetValidatorsResponse;
use helix_common::api::proposer_api::ValidatorRegistrationInfo;
use helix_common::{
    api::builder_api::BuilderGetValidatorsResponseEntry, bid_submission::SignedBidSubmission,
    fork_info::ForkInfo, signing::RelaySigningContext, simulator::BlockSimError, SubmissionTrace,
};
use helix_utils::{calculate_withdrawals_root, has_reached_fork};

use crate::{
    builder::{error::BuilderApiError, traits::BlockSimulator, BlockSimRequest, SubmitBlockParams},
    gossiper::traits::GossipClientTrait,
};

pub const MAX_PAYLOAD_LENGTH: usize = 1024 * 1024 * 4;

#[derive(Clone)]
pub enum DbInfo {
    NewSubmission(Arc<SignedBidSubmission>),
    SimulationResult { block_hash: ByteVector<32>, block_sim_result: Result<(), BlockSimError> },
}

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
    fork_info: Arc<ForkInfo>,
    simulator: S,
    gossiper: Arc<G>,
    signing_context: Arc<RelaySigningContext>,

    db_sender: Sender<DbInfo>,

    curr_slot: Arc<AtomicU64>,
    proposer_duties_response: Arc<RwLock<Option<Vec<u8>>>>,
    next_proposer_duty: Arc<RwLock<Option<BuilderGetValidatorsResponseEntry>>>,
    payload_attributes: Arc<RwLock<HashMap<Bytes32, PayloadAttributesUpdate>>>,
    seen_block_hashes: Arc<RwLock<HashSet<Hash32>>>,
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
        fork_info: Arc<ForkInfo>,
        simulator: S,
        gossiper: Arc<G>,
        signing_context: Arc<RelaySigningContext>,
        slot_update_subscription: Sender<Sender<ChainUpdate>>,
    ) -> Self {
        let (db_sender, db_receiver) = mpsc::channel::<DbInfo>(1000);

        // Spin up db processing task
        let db_clone = db.clone();
        tokio::spawn(async move {
            process_db_additions(db_clone, db_receiver).await;
        });

        let api = Self {
            auctioneer,
            db,
            fork_info,
            simulator,
            gossiper,
            signing_context,

            db_sender,

            curr_slot: AtomicU64::new(0).into(),
            proposer_duties_response: Arc::new(RwLock::new(None)),
            next_proposer_duty: Arc::new(RwLock::new(None)),
            payload_attributes: Arc::new(RwLock::new(HashMap::new())),
            seen_block_hashes: Arc::new(RwLock::new(HashSet::new())),
        };

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

    async fn block_hash_seen_or_insert(&self, block_hash: &Hash32) -> bool {
        let mut seen_block_hashes = self.seen_block_hashes.write().await;
        !seen_block_hashes.insert(block_hash.clone())
    }

    async fn clear_seen_block_hashes(&self) {
        self.seen_block_hashes.write().await.clear();
    }

    /// Implements this API: https://flashbots.github.io/relay-specs/#/Builder/getValidators
    pub async fn get_validators(
        Extension(api): Extension<Arc<BuilderApi<A, DB, S, G>>>,
    ) -> impl IntoResponse {
        let duty_bytes = api.proposer_duties_response.read().await;
        match &*duty_bytes {
            Some(bytes) => Response::builder()
                .status(StatusCode::OK)
                .body(hyper::Body::from(bytes.clone()))
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
    /// Implements this API: https://flashbots.github.io/relay-specs/#/Builder/submitBlock
    pub async fn submit_block(
        Extension(api): Extension<Arc<BuilderApi<A, DB, S, G>>>,
        req: Request<Body>,
    ) -> Result<StatusCode, BuilderApiError> {
        let request_id = Uuid::new_v4();
        let mut trace = SubmissionTrace { receive: get_nanos_timestamp()?, ..Default::default() };

        // Extract the submit block params from the axum Request
        let params = extract_params_from_request(req).await?;

        // Send the request to other instances using the GossipClient
        if let Err(e) = api.gossiper.broadcast_block(&params).await {
            error!("Failed to gossip block submission request: {:?}", e);
        }

        let res: Result<(StatusCode, ByteVector<32>), BuilderApiError> =
            api._submit_block(params, &mut trace, request_id).await;
        match res {
            Ok((status, block_hash)) => {
                // Save latency trace to db
                tokio::spawn(async move {
                    if let Err(err) = api.db.save_block_submission_trace(block_hash, trace).await {
                        error!(error = %err, "Failed to save submission trace");
                    }
                });
                Ok(status)
            }
            Err(err) => {
                error!(error = %err, "failed to submit block");
                Err(err)
            }
        }
    }

    pub async fn process_gossiped_submit_block(
        api: Arc<BuilderApi<A, DB, S, G>>,
        req: SubmitBlockParams,
    ) -> Result<StatusCode, BuilderApiError> {
        let request_id = Uuid::new_v4();
        let mut trace = SubmissionTrace { receive: get_nanos_timestamp()?, ..Default::default() };

        let res: Result<(StatusCode, ByteVector<32>), BuilderApiError> =
            api._submit_block(req, &mut trace, request_id).await;

        match res {
            Ok((status, block_hash)) => {
                // Save latency trace to db
                tokio::spawn(async move {
                    if let Err(err) = api.db.save_block_submission_trace(block_hash, trace).await {
                        error!(error = %err, "Failed to save submission trace");
                    }
                });
                Ok(status)
            }
            Err(err) => {
                error!(error = %err, "failed to submit block");
                Err(err)
            }
        }
    }

    async fn _submit_block(
        &self,
        req: SubmitBlockParams,
        trace: &mut SubmissionTrace,
        request_id: Uuid,
    ) -> Result<(StatusCode, Hash32), BuilderApiError> {
        let head_slot = self.curr_slot.load(Ordering::Relaxed);

        info!(
            request_id = %request_id,
            event = "submit_block",
            head_slot = head_slot,
            timestamp_request_start = trace.receive,
        );

        // Decode the incoming request body into a payload
        let (mut payload, is_cancellations_enabled) =
            decode_payload(req, trace, &request_id).await?;
        let block_hash = payload.message.block_hash.clone();

        if self.block_hash_seen_or_insert(&block_hash).await {
            warn!(request_id = %request_id, block_hash = ?block_hash, "duplicate block hash");
            return Err(BuilderApiError::DuplicateBlockHash { block_hash });
        }

        debug!(
            request_id = %request_id,
            builder_pubkey = ?payload.builder_public_key(),
            block_value = %payload.value(),
            "payload decoded",
        );

        // Fetch the next proposer duty/ payload attributes and validate basic information about the payload
        let (next_duty, payload_attributes) =
            self.fetch_proposer_and_attributes(&payload, &request_id).await?;
        if let Err(err) = sanity_check_block_submission(
            &payload,
            &next_duty,
            head_slot,
            &payload_attributes,
            &self.fork_info,
        ) {
            warn!(request_id = %request_id, error = %err, "failed sanity check");
            return Err(err);
        }
        trace.pre_checks = get_nanos_timestamp()?;

        // Verify the payload signature
        if let Err(err) = payload.verify_signature(&self.fork_info.context) {
            warn!(request_id = %request_id, error = %err, "failed to verify signature");
            return Err(BuilderApiError::SignatureVerificationFailed);
        }
        trace.signature = get_nanos_timestamp()?;

        // Verify payload has not already been delivered
        match self.auctioneer.get_last_slot_delivered().await {
            Ok(Some(slot)) => {
                if payload.slot() <= slot {
                    warn!(request_id = %request_id, "payload already delivered");
                    return Err(BuilderApiError::PayloadAlreadyDelivered);
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!(request_id = %request_id, error = %err, "failed to get last slot delivered");
            }
        }

        let payload = Arc::new(payload);

        // Save submission to db.
        self.db_sender
            .send(DbInfo::NewSubmission(payload.clone()))
            .await
            .map_err(|_| BuilderApiError::InternalError)?;

        // Verify the payload value is above the floor bid
        let floor_bid_value = self
            .check_if_bid_is_below_floor(&payload, is_cancellations_enabled, trace, &request_id)
            .await?;

        // Simulate the submission
        self.simulate_submission(payload.clone(), trace, next_duty.entry, &request_id).await?;

        // If cancellations are enabled, then abort now if there is a later submission
        if is_cancellations_enabled {
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
                    if trace.receive < latest_payload_received_at {
                        return Err(BuilderApiError::AlreadyProcessingNewerPayload);
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    error!(request_id = %request_id, error = %err, "failed to get last slot delivered");
                }
            }
        }

        // Save bid to auctioneer
        self.save_bid_to_auctioneer(
            payload.clone(),
            trace,
            is_cancellations_enabled,
            floor_bid_value,
            &request_id,
        )
        .await?;

        // Log some final info
        trace.request_finish = get_nanos_timestamp()?;
        info!(
            request_id = %request_id,
            trace = ?trace,
            request_duration_ns = trace.receive - trace.request_finish,
            "request finished"
        );

        Ok((StatusCode::OK, block_hash))
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
    /// Checks if the bid in the payload is below the floor value.
    ///
    /// - If cancellations are enabled and the bid is below the floor, it deletes the previous bid.
    /// - If cancellations are not enabled and the bid is at or below the floor, then skip the submission.
    ///
    /// Returns the floor bid value.
    async fn check_if_bid_is_below_floor(
        &self,
        payload: &SignedBidSubmission,
        is_cancellations_enabled: bool,
        trace: &mut SubmissionTrace,
        request_id: &Uuid,
    ) -> Result<U256, BuilderApiError> {
        let floor_bid_value = match self
            .auctioneer
            .get_floor_bid_value(
                payload.slot(),
                payload.parent_hash(),
                payload.proposer_public_key(),
            )
            .await
        {
            Ok(floor_value) => floor_value.unwrap_or(U256::ZERO),
            Err(err) => {
                error!(request_id = %request_id, error = %err, "Failed to get floor bid value");
                return Err(BuilderApiError::InternalError);
            }
        };

        let is_bid_below_floor = payload.value() < floor_bid_value;
        let is_bid_at_or_below_floor = payload.value() <= floor_bid_value;

        if is_cancellations_enabled && is_bid_below_floor {
            debug!(request_id = %request_id, "submission below floor bid value, with cancellation");
            if let Err(err) = self
                .auctioneer
                .delete_builder_bid(
                    payload.slot(),
                    payload.parent_hash(),
                    payload.proposer_public_key(),
                    payload.builder_public_key(),
                )
                .await
            {
                error!(
                    request_id = %request_id,
                    error = %err,
                    "Failed processing cancellable bid below floor. Could not delete builder bid.",
                );
                return Err(BuilderApiError::InternalError);
            }
            return Err(BuilderApiError::BidBelowFloor);
        } else if !is_cancellations_enabled && is_bid_at_or_below_floor {
            debug!(request_id = %request_id, "submission at or below floor bid value, without cancellation");
            return Err(BuilderApiError::BidBelowFloor);
        }
        trace.floor_bid_checks = get_nanos_timestamp()?;
        Ok(floor_bid_value)
    }

    /// Simulates a new block payload.
    ///
    /// 1. Checks the current top bid value from the auctioneer.
    /// 3. Invokes the block simulator for validation.
    async fn simulate_submission(
        &self,
        payload: Arc<SignedBidSubmission>,
        trace: &mut SubmissionTrace,
        registration_info: ValidatorRegistrationInfo,
        request_id: &Uuid,
    ) -> Result<(), BuilderApiError> {
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
        );
        let result = self
            .simulator
            .process_request(sim_request, is_top_bid, self.db_sender.clone(), *request_id)
            .await;
        if let Err(err) = result {
            return match &err {
                BlockSimError::BlockValidationFailed(reason) => {
                    warn!(request_id = %request_id, error = %reason, "block validation failed");
                    Err(BuilderApiError::BlockValidationError(err))
                }
                _ => {
                    error!(request_id = %request_id, error = %err, "error simulating block");
                    Err(BuilderApiError::InternalError)
                }
            };
        } else {
            info!(request_id = %request_id, "block simulation successful");
        }

        trace.simulation = get_nanos_timestamp()?;
        debug!(request_id = %request_id, sim_latency = trace.simulation - trace.signature);

        Ok(())
    }

    async fn save_bid_to_auctioneer(
        &self,
        payload: Arc<SignedBidSubmission>,
        trace: &mut SubmissionTrace,
        is_cancellations_enabled: bool,
        floor_bid_value: U256,
        request_id: &Uuid,
    ) -> Result<(), BuilderApiError> {
        let mut update_bid_result = SaveBidAndUpdateTopBidResponse::default();
        let result = self
            .auctioneer
            .save_bid_and_update_top_bid(
                payload,
                trace.receive.into(),
                is_cancellations_enabled,
                floor_bid_value,
                &mut update_bid_result,
                &self.signing_context,
            )
            .await;

        if let Err(err) = result {
            error!(request_id = %request_id, error = %err, "could not save bid and update top bids");
            return Err(BuilderApiError::AuctioneerError(err));
        }

        // Log the results of the bid submission
        trace.auctioneer_update = get_nanos_timestamp()?;
        info!(
            request_id = %request_id,
            bid_update_latency = trace.auctioneer_update - trace.simulation,
            was_bid_saved_in = update_bid_result.was_bid_saved,
            was_top_bid_updated = update_bid_result.was_top_bid_updated,
            top_bid_value = ?update_bid_result.top_bid_value,
            prev_top_bid_value = ?update_bid_result.prev_top_bid_value,
            save_payload = update_bid_result.latency_save_payload,
            update_top_bid = update_bid_result.latency_update_top_bid,
            update_floor = update_bid_result.latency_update_floor,
        );

        if update_bid_result.was_bid_saved {
            debug!(request_id = %request_id, eligible_at = get_nanos_timestamp()?);
        }

        Ok(())
    }

    async fn fetch_proposer_and_attributes(
        &self,
        payload: &SignedBidSubmission,
        request_id: &Uuid,
    ) -> Result<(BuilderGetValidatorsResponseEntry, PayloadAttributesUpdate), BuilderApiError> {
        let next_proposer_duty = self.next_proposer_duty.read().await.clone().ok_or_else(|| {
            warn!(request_id = %request_id, "could not find slot duty");
            BuilderApiError::ProposerDutyNotFound
        })?;

        let payload_attributes =
            self.payload_attributes.read().await.get(payload.parent_hash()).cloned().ok_or_else(
                || {
                    warn!(request_id = %request_id, "payload attributes not yet known");
                    BuilderApiError::PayloadAttributesNotYetKnown
                },
            )?;

        if payload_attributes.slot != payload.slot() {
            warn!(request_id = %request_id, "payload attributes not yet known");
            return Err(BuilderApiError::PayloadSlotMismatchWithPayloadAttributes {
                got: payload.slot(),
                expected: payload_attributes.slot,
            });
        }

        Ok((next_proposer_duty, payload_attributes))
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

        self.curr_slot.store(slot_update.slot, Ordering::Relaxed);
        *self.next_proposer_duty.write().await = slot_update.next_duty;

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

        self.clear_seen_block_hashes().await;
    }

    async fn handle_new_payload_attributes(&self, payload_attributes: PayloadAttributesUpdate) {
        let head_slot = self.curr_slot.load(Ordering::Relaxed);

        debug!(
            randao = ?payload_attributes.payload_attributes.prev_randao,
            timestamp = payload_attributes.payload_attributes.timestamp,
        );

        // Clean up old payload attributes
        let mut all_payload_attributes = self.payload_attributes.write().await;
        all_payload_attributes.retain(|_, value| value.slot < head_slot);

        // Save new one
        all_payload_attributes.insert(payload_attributes.parent_hash.clone(), payload_attributes);
    }
}

/// `extract_params_from_request` extracts all necessary params from a `Request<Body>`.
///
/// - Check for cancellations.
/// - Check for gzip compression.
/// - Check for SSZ encoding.
async fn extract_params_from_request(
    req: Request<Body>,
) -> Result<SubmitBlockParams, BuilderApiError> {
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
    let body_bytes = hyper::body::to_bytes(body).await?;
    if body_bytes.len() > MAX_PAYLOAD_LENGTH {
        return Err(BuilderApiError::PayloadTooLarge {
            max_size: MAX_PAYLOAD_LENGTH,
            size: body_bytes.len(),
        });
    }

    Ok(SubmitBlockParams { body_bytes, is_cancellations_enabled, is_gzip, is_ssz })
}

/// `decode_payload` decodes the payload from `SubmitBlockParams` into a `SignedBidSubmission` object.
///
/// - Supports both SSZ and JSON encodings for deserialization.
/// - Automatically falls back to JSON if SSZ deserialization fails.
/// - Handles GZIP-compressed payloads.
///
/// It returns a tuple of the decoded payload, if cancellations are enabled and the time it took to decode.
async fn decode_payload(
    mut req: SubmitBlockParams,
    trace: &mut SubmissionTrace,
    request_id: &Uuid,
) -> Result<(SignedBidSubmission, bool), BuilderApiError> {
    // Decompress if necessary
    if req.is_gzip {
        let mut decoder = GzDecoder::new(&req.body_bytes[..]);

        // TODO: profile this. 2 is a guess.
        let estimated_size = (req.body_bytes.len() * 2) as usize;
        let mut buf = Vec::with_capacity(estimated_size);

        decoder.read_to_end(&mut buf)?;
        req.body_bytes = buf.into();
    }

    // Decode payload
    let payload: SignedBidSubmission = if req.is_ssz {
        match ssz::prelude::deserialize(&req.body_bytes) {
            Ok(payload) => payload,
            Err(err) => {
                println!("Failed to decode ssz payload: {err}");
                // Fallback to JSON
                warn!(request_id = %request_id, error = %err, "Failed to decode payload using SSZ; falling back to JSON");
                serde_json::from_slice(&req.body_bytes)?
            }
        }
    } else {
        serde_json::from_slice(&req.body_bytes)?
    };

    trace.decode = get_nanos_timestamp()?;
    info!(
        request_id = %request_id,
        timestamp_after_decoding = Instant::now().elapsed().as_nanos(),
        decode_latency_ns = trace.decode - trace.receive,
        builder_pubkey = ?payload.builder_public_key(),
        block_hash = ?payload.block_hash(),
        proposer_pubkey = ?payload.proposer_public_key(),
        parent_hash = ?payload.parent_hash(),
        value = ?payload.value(),
        num_tx = payload.execution_payload.transactions().len(),
    );

    Ok((payload, req.is_cancellations_enabled))
}

/// - Validates the slot timing against the current head slot.
/// - Validates the expected block.timestamp.
/// - Ensures that the fee recipients in the payload and proposer duty match.
/// - Ensures that the slot in the payload and payload attributes match.
/// - Validates that the block hash in the payload and message are the same.
/// - Validates that the parent hash in the payload and message are the same.
fn sanity_check_block_submission(
    payload: &SignedBidSubmission,
    next_duty: &BuilderGetValidatorsResponseEntry,
    head_slot: u64,
    payload_attributes: &PayloadAttributesUpdate,
    fork_info: &ForkInfo,
) -> Result<(), BuilderApiError> {
    if payload.slot() <= head_slot {
        return Err(BuilderApiError::SubmissionForPastSlot {
            current_slot: head_slot,
            submission_slot: payload.slot(),
        });
    }

    let expected_timestamp =
        fork_info.genesis_time_in_secs + (payload.message.slot * SECONDS_PER_SLOT);
    if payload.execution_payload.timestamp() != expected_timestamp {
        return Err(BuilderApiError::IncorrectTimestamp {
            got: payload.execution_payload.timestamp(),
            expected: expected_timestamp,
        });
    }

    // Check duty
    if next_duty.entry.registration.message.fee_recipient != *payload.proposer_fee_recipient() {
        return Err(BuilderApiError::FeeRecipientMismatch {
            got: payload.proposer_fee_recipient().clone(),
            expected: next_duty.entry.registration.message.fee_recipient.clone(),
        });
    }

    if payload.slot() != next_duty.slot {
        return Err(BuilderApiError::SlotMismatch {
            got: payload.slot(),
            expected: next_duty.slot,
        });
    }

    // Check payload attrs
    if *payload.execution_payload.prev_randao() != payload_attributes.payload_attributes.prev_randao
    {
        return Err(BuilderApiError::PrevRandaoMismatch {
            got: payload.execution_payload.prev_randao().clone(),
            expected: payload_attributes.payload_attributes.prev_randao.clone(),
        });
    }

    if has_reached_fork(payload.slot(), CAPELLA_FORK_EPOCH) {
        let payload_withdrawals = match payload.execution_payload.withdrawals() {
            Some(w) => w,
            None => return Err(BuilderApiError::MissingWithdrawls),
        };

        let withdrawals_root = calculate_withdrawals_root(payload_withdrawals);
        let expected_withdrawals_root = match payload_attributes.withdrawals_root {
            Some(wr) => wr,
            None => return Err(BuilderApiError::MissingWithdrawls),
        };

        if withdrawals_root != expected_withdrawals_root {
            return Err(BuilderApiError::WithdrawalsRootMismatch {
                got: withdrawals_root,
                expected: expected_withdrawals_root,
            });
        }
    }

    // Misc sanity checks
    if payload.value() == U256::ZERO || payload.execution_payload.transactions().is_empty() {
        return Err(BuilderApiError::ZeroValueBlock);
    }

    if payload.message.block_hash != *payload.execution_payload.block_hash() {
        return Err(BuilderApiError::BlockHashMismatch {
            message: payload.message.block_hash.clone(),
            payload: payload.execution_payload.block_hash().clone(),
        });
    }

    if payload.message.parent_hash != *payload.execution_payload.parent_hash() {
        return Err(BuilderApiError::ParentHashMismatch {
            message: payload.message.parent_hash.clone(),
            payload: payload.execution_payload.parent_hash().clone(),
        });
    }

    Ok(())
}

/// Should be called as a new async task.
/// Stores updates to the db out of the critical path.
async fn process_db_additions<DB: DatabaseService + 'static>(
    db: Arc<DB>,
    mut db_receiver: mpsc::Receiver<DbInfo>,
) {
    while let Some(db_info) = db_receiver.recv().await {
        match db_info {
            DbInfo::NewSubmission(submission) => {
                if let Err(err) = db.store_block_submission(submission).await {
                    error!(
                        error = %err,
                        "failed to store block submission",
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

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use hyper::http::header::{HeaderValue, CONTENT_ENCODING, CONTENT_TYPE};
    use hyper::http::uri::Uri;
    use std::io::Write;
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
    async fn test_decode_ssz_payload_gzip() {
        let ssz_payload = vec![];
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&ssz_payload).unwrap();
        let compressed_payload = encoder.finish().unwrap();

        let req = build_test_request(compressed_payload, true, true).await;
        let mut trace = create_test_submission_trace().await;
        let request_id = create_test_uuid().await;

        let req = extract_params_from_request(req).await.unwrap();
        let result = decode_payload(req, &mut trace, &request_id).await.unwrap();
        assert!(!result.1); // Assert that cancellations are enabled
    }

    #[tokio::test]
    async fn test_decode_payload_too_large() {
        let payload = vec![0u8; MAX_PAYLOAD_LENGTH + 1];
        let req = build_test_request(payload, false, false).await;
        let mut trace = create_test_submission_trace().await;
        let request_id = create_test_uuid().await;

        let req = extract_params_from_request(req).await.unwrap();
        let result = decode_payload(req, &mut trace, &request_id).await;
        assert!(matches!(result, Err(BuilderApiError::PayloadTooLarge { .. })));
    }
}
