use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{to_bytes, Body},
    extract::{Json, Path},
    http::{HeaderMap, Request, StatusCode},
    response::IntoResponse,
    Extension,
};
use ethereum_consensus::{
    builder::SignedValidatorRegistration,
    clock::get_current_unix_time_in_nanos,
    deneb::{Context, Root},
    phase0::mainnet::SLOTS_PER_EPOCH,
    primitives::BlsPublicKey,
    ssz::prelude::*,
    types::mainnet::{
        ExecutionPayloadHeader, ExecutionPayloadHeaderRef, SignedBeaconBlock,
        SignedBlindedBeaconBlock,
    },
};

use tokio::{
    sync::{
        mpsc::{self, error::SendError, Receiver, Sender},
        RwLock,
    },
    time::{sleep, Instant},
};
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

use helix_beacon_client::{types::BroadcastValidation, BlockBroadcaster, MultiBeaconClientTrait};
use helix_common::{
    api::{
        builder_api::BuilderGetValidatorsResponseEntry,
        proposer_api::{GetPayloadResponse, ValidatorRegistrationInfo},
    },
    beacon_api::PublishBlobsRequest,
    chain_info::{ChainInfo, Network},
    deneb::{BlobSidecars, BuildBlobSidecarError},
    signed_proposal::VersionedSignedProposal,
    try_execution_header_from_payload,
    versioned_payload::PayloadAndBlobs,
    BidRequest, Filtering, GetHeaderTrace, GetPayloadTrace, RegisterValidatorsTrace,
    ValidatorPreferences,
};
use helix_database::DatabaseService;
use helix_datastore::{error::AuctioneerError, Auctioneer};
use helix_housekeeper::{ChainUpdate, SlotUpdate};
use helix_utils::signing::{verify_signed_builder_message, verify_signed_consensus_message};

use crate::{
    gossiper::{
        traits::GossipClientTrait,
        types::{BroadcastGetPayloadParams, GossipedMessage},
    },
    proposer::{
        error::ProposerApiError, unblind_beacon_block, GetHeaderParams, PreferencesHeader,
        GET_HEADER_REQUEST_CUTOFF_MS,
    },
};

const GET_PAYLOAD_REQUEST_CUTOFF_MS: i64 = 4000;
pub(crate) const MAX_BLINDED_BLOCK_LENGTH: usize = 1024 * 1024;
pub const MAX_VAL_REGISTRATIONS_LENGTH: usize = 425 * 10_000; // 425 bytes per registration (json) * 10,000 registrations

#[derive(Clone)]
pub struct ProposerApi<A, DB, M, G>
where
    A: Auctioneer,
    DB: DatabaseService,
    M: MultiBeaconClientTrait,
    G: GossipClientTrait + 'static,
{
    auctioneer: Arc<A>,
    db: Arc<DB>,
    gossiper: Arc<G>,
    broadcasters: Vec<Arc<BlockBroadcaster>>,
    multi_beacon_client: Arc<M>,

    /// Information about the current head slot and next proposer duty
    curr_slot_info: Arc<RwLock<(u64, Option<BuilderGetValidatorsResponseEntry>)>>,

    chain_info: Arc<ChainInfo>,
    validator_preferences: Arc<ValidatorPreferences>,

    target_get_payload_propagation_duration_ms: u64,
}

impl<A, DB, M, G> ProposerApi<A, DB, M, G>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
    M: MultiBeaconClientTrait + 'static,
    G: GossipClientTrait + 'static,
{
    pub fn new(
        auctioneer: Arc<A>,
        db: Arc<DB>,
        gossiper: Arc<G>,
        broadcasters: Vec<Arc<BlockBroadcaster>>,
        multi_beacon_client: Arc<M>,
        chain_info: Arc<ChainInfo>,
        slot_update_subscription: Sender<Sender<ChainUpdate>>,
        validator_preferences: Arc<ValidatorPreferences>,
        target_get_payload_propagation_duration_ms: u64,
        gossip_receiver: Receiver<GossipedMessage>,
    ) -> Self {
        let api = Self {
            auctioneer,
            db,
            gossiper,
            broadcasters,
            multi_beacon_client,
            curr_slot_info: Arc::new(RwLock::new((0, None))),
            chain_info,
            validator_preferences,
            target_get_payload_propagation_duration_ms,
        };

        // Spin up gossip processing task
        let api_clone = api.clone();
        tokio::spawn(async move {
            api_clone.process_gossiped_info(gossip_receiver).await;
        });

        // Spin up the housekeep task
        let api_clone = api.clone();
        tokio::spawn(async move {
            if let Err(err) = api_clone.housekeep(slot_update_subscription).await {
                error!(
                    error = %err,
                    "ProposerApi. housekeep task encountered an error",
                );
            }
        });

        api
    }

    /// Implements this API: <https://ethereum.github.io/builder-specs/#/Builder/status>
    pub async fn status(
        Extension(_proposer_api): Extension<Arc<ProposerApi<A, DB, M, G>>>,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        Ok(StatusCode::OK)
    }

    /// Registers a batch of validators to the relay.
    ///
    /// This function accepts a list of `SignedValidatorRegistration` objects and performs the
    /// following steps:
    /// 1. Validates the registration timestamp of each validator.
    /// 2. Checks if the validator is known in the validator registry.
    /// 3. Verifies the signature of each registration.
    /// 4. Writes validated registrations to the registry.
    ///
    /// If all registrations in the batch fail validation, an error is returned.
    ///
    /// Implements this API: <https://ethereum.github.io/builder-specs/#/Builder/registerValidator>
    pub async fn register_validators(
        Extension(proposer_api): Extension<Arc<ProposerApi<A, DB, M, G>>>,
        headers: HeaderMap,
        Json(registrations): Json<Vec<SignedValidatorRegistration>>,
    ) -> Result<StatusCode, ProposerApiError> {
        if registrations.is_empty() {
            return Err(ProposerApiError::EmptyRequest)
        }

        // Get optional api key from headers
        let api_key = headers.get("x-api-key").and_then(|key| key.to_str().ok());

        let pool_name = match api_key {
            Some(api_key) => match proposer_api.db.get_validator_pool_name(api_key).await? {
                Some(pool_name) => Some(pool_name),
                None => {
                    warn!("Invalid api key provided");
                    return Err(ProposerApiError::InvalidApiKey)
                }
            },
            None => None,
        };

        // Set using default preferences from config
        let mut validator_preferences = ValidatorPreferences {
            filtering: proposer_api.validator_preferences.filtering,
            trusted_builders: proposer_api.validator_preferences.trusted_builders.clone(),
            header_delay: proposer_api.validator_preferences.header_delay,
            gossip_blobs: proposer_api.validator_preferences.gossip_blobs,
        };

        let preferences_header = headers.get("x-preferences");
        let preferences = match preferences_header {
            Some(preferences_header) => {
                let decoded_prefs: PreferencesHeader =
                    serde_json::from_str(preferences_header.to_str()?)?;
                Some(decoded_prefs)
            }
            None => None,
        };

        if let Some(preferences) = preferences {
            // Overwrite preferences if they are provided

            if let Some(filtering) = preferences.filtering {
                validator_preferences.filtering = filtering;
            } else if let Some(censoring) = preferences.censoring {
                validator_preferences.filtering = match censoring {
                    true => Filtering::Regional,
                    false => Filtering::Global,
                };
            }

            if let Some(trusted_builders) = preferences.trusted_builders {
                validator_preferences.trusted_builders = Some(trusted_builders);
            }

            if let Some(header_delay) = preferences.header_delay {
                validator_preferences.header_delay = header_delay;
            }

            if let Some(gossip_blobs) = preferences.gossip_blobs {
                validator_preferences.gossip_blobs = gossip_blobs;
            }
        }

        let request_id = Uuid::new_v4();
        let mut trace =
            RegisterValidatorsTrace { receive: get_nanos_timestamp()?, ..Default::default() };

        let (head_slot, _) = *proposer_api.curr_slot_info.read().await;
        let num_registrations = registrations.len();
        debug!(
            request_id = %request_id,
            event = "register_validators",
            head_slot = head_slot,
            num_registrations = num_registrations,
        );

        // Bulk check if the validators are known
        let registration_pub_keys =
            registrations.iter().map(|r| r.message.public_key.clone()).collect();
        let known_pub_keys = proposer_api.db.check_known_validators(registration_pub_keys).await?;

        // Check each registration
        let mut valid_registrations = Vec::with_capacity(known_pub_keys.len());

        let mut handles = Vec::with_capacity(registrations.len());

        for mut registration in registrations {
            let proposer_api_clone = proposer_api.clone();
            let start_time = Instant::now();

            let pub_key = registration.message.public_key.clone();

            debug!(
                request_id = %request_id,
                pub_key = ?pub_key,
                fee_recipient = %registration.message.fee_recipient,
                gas_limit = registration.message.gas_limit,
                timestamp = registration.message.timestamp,
            );

            if !known_pub_keys.contains(&pub_key) {
                warn!(
                    request_id = %request_id,
                    pub_key = ?pub_key,
                    "Registration for unknown validator",
                );
                continue
            }

            if !proposer_api_clone.db.is_registration_update_required(&registration).await? {
                debug!(
                    request_id = %request_id,
                    pub_key = ?pub_key,
                    "Registration update not required",
                );
                valid_registrations.push(registration);
                continue
            }

            let handle = tokio::task::spawn_blocking(move || {
                let res = match proposer_api_clone.validate_registration(&mut registration) {
                    Ok(_) => Some(registration),
                    Err(err) => {
                        warn!(
                            request_id = %request_id,
                            err = %err,
                            pub_key = ?pub_key,
                            "Failed to register validator",
                        );
                        None
                    }
                };

                trace!(
                    request_id = %request_id,
                    pub_key = ?pub_key,
                    elapsed_time = %start_time.elapsed().as_nanos(),
                );

                res
            });
            handles.push(handle);
        }

        for handle in handles {
            let reg = handle.await.map_err(|_| ProposerApiError::InternalServerError)?;
            if let Some(reg) = reg {
                valid_registrations.push(reg);
            }
        }
        trace.registrations_complete = get_nanos_timestamp()?;

        let successful_registrations = valid_registrations.len();

        // Add validator preferences to each registration
        let mut valid_registrations_infos = Vec::new();

        for reg in valid_registrations {
            let mut preferences = validator_preferences.clone();

            if proposer_api.auctioneer.is_primev_proposer(&reg.message.public_key).await? {
                preferences.trusted_builders = Some(vec!["PrimevBuilder".to_string()]);
            }

            valid_registrations_infos
                .push(ValidatorRegistrationInfo { registration: reg, preferences });
        }

        // Bulk write registrations to db
        tokio::spawn(async move {
            if let Err(err) = proposer_api
                .db
                .save_validator_registrations(valid_registrations_infos, pool_name)
                .await
            {
                error!(
                    request_id = %request_id,
                    err = %err,
                    "failed to save validator registrations",
                );
            }
        });

        info!(
            request_id = %request_id,
            trace = ?trace,
            successful_registrations = successful_registrations,
            failed_registrations = num_registrations - successful_registrations,
        );

        Ok(StatusCode::OK)
    }

    /// Retrieves the best bid header for the specified slot, parent hash, and public key.
    ///
    /// This function accepts a slot number, parent hash and public_key.
    /// 1. Validates that the request's slot is not older than the head slot.
    /// 2. Validates the request timestamp to ensure it's not too late.
    /// 3. Fetches the best bid for the given parameters from the auctioneer.
    ///
    /// The function returns a JSON response containing the best bid if found.
    ///
    /// Implements this API: <https://ethereum.github.io/builder-specs/#/Builder/getHeader>
    pub async fn get_header(
        Extension(proposer_api): Extension<Arc<ProposerApi<A, DB, M, G>>>,
        headers: HeaderMap,
        Path(GetHeaderParams { slot, parent_hash, public_key }): Path<GetHeaderParams>,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        if proposer_api.auctioneer.kill_switch_enabled().await? {
            return Err(ProposerApiError::ServiceUnavailableError)
        }

        let request_id = Uuid::new_v4();
        let mut trace = GetHeaderTrace { receive: get_nanos_timestamp()?, ..Default::default() };

        let (head_slot, duty) = proposer_api.curr_slot_info.read().await.clone();
        debug!(
            request_id = %request_id,
            event = "get_header",
            head_slot = head_slot,
            request_ts = trace.receive,
            slot = slot,
            parent_hash = ?parent_hash,
            public_key = ?public_key,
        );

        let bid_request = BidRequest { slot, parent_hash, public_key };

        // Dont allow requests for past slots
        if bid_request.slot < head_slot {
            warn!(request_id = %request_id, "request for past slot");
            return Err(ProposerApiError::RequestForPastSlot {
                request_slot: bid_request.slot,
                head_slot,
            })
        }

        // Only return a bid if there is a proposer connected this slot.
        if duty.is_none() {
            debug!(%request_id, "proposer duty not found");
            return Err(ProposerApiError::ProposerNotRegistered)
        }
        let _duty = duty.unwrap();

        let _ms_into_slot = match proposer_api.validate_bid_request_time(&bid_request) {
            Ok(ms_into_slot) => ms_into_slot,
            Err(err) => {
                warn!(request_id = %request_id, err = %err, "invalid bid request time");
                return Err(err)
            }
        };
        trace.validation_complete = get_nanos_timestamp()?;

        let user_agent =
            headers.get("user-agent").and_then(|v| v.to_str().ok()).map(|v| v.to_string());

        // Get best bid from auctioneer
        let get_best_bid_res = proposer_api
            .auctioneer
            .get_best_bid(bid_request.slot, &bid_request.parent_hash, &bid_request.public_key)
            .await;
        trace.best_bid_fetched = get_nanos_timestamp()?;
        info!(request_id = %request_id, trace = ?trace, "best bid fetched");

        match get_best_bid_res {
            Ok(Some(bid)) => {
                if bid.value() == U256::ZERO {
                    warn!(request_id = %request_id, "best bid value is 0");
                    return Err(ProposerApiError::BidValueZero)
                }

                info!(
                    request_id = %request_id,
                    value = ?bid.value(),
                    block_hash = ?bid.block_hash(),
                    "delivering bid",
                );

                // Save trace to DB
                proposer_api
                    .save_get_header_call(
                        slot,
                        bid_request.parent_hash,
                        bid_request.public_key,
                        bid.block_hash().clone(),
                        trace,
                        request_id,
                        user_agent,
                    )
                    .await;

                // Return header
                Ok(axum::Json(bid))
            }
            Ok(None) => {
                warn!(request_id = %request_id, "no bid found");
                Err(ProposerApiError::NoBidPrepared)
            }
            Err(err) => {
                error!(request_id = %request_id, error = %err, "error getting bid");
                Err(ProposerApiError::InternalServerError)
            }
        }
    }

    /// Retrieves the execution payload for a given blinded beacon block.
    ///
    /// This function accepts a `SignedBlindedBeaconBlock` as input and performs several steps:
    /// 1. Validates the proposer index and verifies the block's signature.
    /// 2. Retrieves the corresponding execution payload from the auctioneer.
    /// 3. Validates the payload and publishes it to the multi-beacon client.
    /// 4. Optionally broadcasts the payload to `broadcasters` (e.g., bloXroute, Fiber).
    /// 5. Stores the delivered payload information to database.
    /// 6. Returns the unblinded payload to proposer.
    ///
    /// Implements this API: <https://ethereum.github.io/builder-specs/#/Builder/submitBlindedBlock>
    pub async fn get_payload(
        Extension(proposer_api): Extension<Arc<ProposerApi<A, DB, M, G>>>,
        _headers: HeaderMap,
        req: Request<Body>,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        let mut trace = GetPayloadTrace { receive: get_nanos_timestamp()?, ..Default::default() };
        let request_id = Uuid::new_v4();

        let signed_blinded_block: SignedBlindedBeaconBlock =
            match deserialize_get_payload_bytes(req).await {
                Ok(signed_block) => signed_block,
                Err(err) => {
                    warn!(
                        request_id = %request_id,
                        event = "get_payload",
                        error = %err,
                        "failed to deserialize signed block",
                    );
                    return Err(err)
                }
            };
        let block_hash =
            signed_blinded_block.message().body().execution_payload_header().block_hash().clone();

        let slot = signed_blinded_block.message().slot();

        // Broadcast get payload request
        if let Err(e) = proposer_api
            .gossiper
            .broadcast_get_payload(BroadcastGetPayloadParams {
                signed_blinded_beacon_block: signed_blinded_block.clone(),
                request_id,
            })
            .await
        {
            error!(request_id = %request_id, error = %e, "failed to broadcast get payload");
        };

        match proposer_api._get_payload(signed_blinded_block, &mut trace, &request_id).await {
            Ok(get_payload_response) => Ok(axum::Json(get_payload_response)),
            Err(err) => {
                // Save error to DB
                if let Err(err) = proposer_api
                    .db
                    .save_failed_get_payload(slot, block_hash, err.to_string(), trace)
                    .await
                {
                    error!(err = ?err, "error saving failed get payload");
                }

                Err(err)
            }
        }
    }

    pub async fn _get_payload(
        &self,
        mut signed_blinded_block: SignedBlindedBeaconBlock,
        trace: &mut GetPayloadTrace,
        request_id: &Uuid,
    ) -> Result<GetPayloadResponse, ProposerApiError> {
        let block_hash =
            signed_blinded_block.message().body().execution_payload_header().block_hash().clone();

        let (head_slot, slot_duty) = self.curr_slot_info.read().await.clone();

        info!(
            request_id = %request_id,
            event = "get_payload",
            head_slot = head_slot,
            request_ts = trace.receive,
            block_hash = ?block_hash,
        );

        // Verify that the request is for the current slot
        if signed_blinded_block.message().slot() <= head_slot {
            warn!(request_id = %request_id, "request for past slot");
            return Err(ProposerApiError::RequestForPastSlot {
                request_slot: signed_blinded_block.message().slot(),
                head_slot,
            })
        }

        // Verify that we have a proposer connected for the current proposal
        if slot_duty.is_none() {
            warn!(request_id = %request_id, "no slot proposer duty");
            return Err(ProposerApiError::ProposerNotRegistered)
        }
        let slot_duty = slot_duty.unwrap();

        if let Err(err) =
            self.validate_proposal_coordinate(&signed_blinded_block, &slot_duty, head_slot).await
        {
            warn!(request_id = %request_id, error = %err, "invalid proposal coordinate");
            return Err(err)
        }
        trace.proposer_index_validated = get_nanos_timestamp()?;

        let proposer_public_key = slot_duty.entry.registration.message.public_key;
        if let Err(err) = self.verify_signed_blinded_block_signature(
            &mut signed_blinded_block,
            &proposer_public_key,
            self.chain_info.genesis_validators_root,
            &self.chain_info.context,
        ) {
            warn!(request_id = %request_id, error = %err, "invalid signature");
            return Err(ProposerApiError::InvalidSignature(err))
        }
        trace.signature_validated = get_nanos_timestamp()?;

        // Get execution payload from auctioneer
        let payload_result = self
            .get_execution_payload(
                signed_blinded_block.message().slot(),
                &proposer_public_key,
                &block_hash,
                request_id,
            )
            .await;

        let mut versioned_payload = match payload_result {
            Ok(p) => p,
            Err(err) => {
                error!(
                    request_id = %request_id,
                    slot = signed_blinded_block.message().slot(),
                    proposer_public_key = ?proposer_public_key,
                    block_hash = ?block_hash,
                    error = %err,
                    "No payload found for slot"
                );
                return Err(ProposerApiError::NoExecutionPayloadFound)
            }
        };
        info!(request_id = %request_id, "found payload for blinded signed block");
        trace.payload_fetched = get_nanos_timestamp()?;

        // Check if get_payload has already been called
        if let Err(err) = self
            .auctioneer
            .check_and_set_last_slot_and_hash_delivered(
                signed_blinded_block.message().slot(),
                &block_hash,
            )
            .await
        {
            match err {
                AuctioneerError::AnotherPayloadAlreadyDeliveredForSlot => {
                    warn!(request_id = %request_id, "validator called get_payload twice for different block hashes");
                    return Err(ProposerApiError::AuctioneerError(err))
                }
                AuctioneerError::PastSlotAlreadyDelivered => {
                    warn!(request_id = %request_id, "validator called get_payload for past slot");
                    return Err(ProposerApiError::AuctioneerError(err))
                }
                _ => {
                    // If error was internal carry on
                    error!(request_id = %request_id, error = %err, "error checking and setting last slot and hash delivered");
                }
            }
        }

        // Handle early/late requests
        if let Err(err) = self
            .await_and_validate_slot_start_time(&signed_blinded_block, trace.receive, request_id)
            .await
        {
            warn!(request_id = %request_id, error = %err, "get_payload was sent too late");

            // Save too late request to db for debugging
            if let Err(db_err) = self
                .db
                .save_too_late_get_payload(
                    signed_blinded_block.message().slot(),
                    &proposer_public_key,
                    &block_hash,
                    trace.receive,
                    trace.payload_fetched,
                )
                .await
            {
                error!(request_id = %request_id, error = %db_err, "failed to save too late get payload");
            }

            return Err(err)
        }

        let message = signed_blinded_block.message();
        let body = message.body();
        let provided_header = body.execution_payload_header();
        let local_header =
            match try_execution_header_from_payload(&mut versioned_payload.execution_payload) {
                Ok(header) => header,
                Err(err) => {
                    error!(
                        request_id = %request_id,
                        error = %err,
                        "error converting execution payload to header",
                    );
                    return Err(err.into())
                }
            };
        if let Err(err) = self.validate_header_equality(&local_header, provided_header) {
            error!(
                request_id = %request_id,
                error = %err,
                "execution payload header invalid, does not match known ExecutionPayload",
            );
            return Err(err)
        }
        trace.validation_complete = get_nanos_timestamp()?;

        let unblinded_payload =
            match unblind_beacon_block(&signed_blinded_block, &versioned_payload) {
                Ok(unblinded_payload) => Arc::new(unblinded_payload),
                Err(err) => {
                    warn!(request_id = %request_id, error = %err, "payload type mismatch");
                    return Err(ProposerApiError::PayloadTypeMismatch)
                }
            };
        let payload = Arc::new(versioned_payload);

        if self.validator_preferences.gossip_blobs ||
            !matches!(self.chain_info.network, Network::Mainnet)
        {
            info!(?request_id, "gossip blobs: about to gossip blobs");
            let self_clone = self.clone();
            let unblinded_payload_clone = unblinded_payload.clone();
            let req_id = *request_id;
            tokio::spawn(async move {
                self_clone.gossip_blobs(unblinded_payload_clone, req_id).await;
            });
        }

        let is_trusted_proposer = self.is_trusted_proposer(&proposer_public_key).await?;

        // Publish and validate payload with multi-beacon-client
        let fork = unblinded_payload.version();
        if is_trusted_proposer {
            let self_clone = self.clone();
            let unblinded_payload_clone = unblinded_payload.clone();
            let request_id_clone = *request_id;
            let mut trace_clone = trace.clone();
            let payload_clone = payload.clone();

            tokio::spawn(async move {
                if let Err(err) = self_clone
                    .multi_beacon_client
                    .publish_block(
                        unblinded_payload_clone.clone(),
                        Some(BroadcastValidation::ConsensusAndEquivocation),
                        fork,
                    )
                    .await
                {
                    error!(request_id = %request_id_clone, error = %err, "error publishing block");
                };

                trace_clone.beacon_client_broadcast = get_nanos_timestamp().unwrap_or_default();

                // Broadcast payload to all broadcasters
                self_clone.broadcast_signed_block(
                    unblinded_payload_clone.clone(),
                    Some(BroadcastValidation::Gossip),
                    &request_id_clone,
                );
                trace_clone.broadcaster_block_broadcast = get_nanos_timestamp().unwrap_or_default();

                // While we wait for the block to propagate, we also store the payload information
                trace_clone.on_deliver_payload = get_nanos_timestamp().unwrap_or_default();
                self_clone
                    .save_delivered_payload_info(
                        payload_clone,
                        &signed_blinded_block,
                        &proposer_public_key,
                        &trace_clone,
                        &request_id_clone,
                    )
                    .await;
            });
        } else {
            if let Err(err) = self
                .multi_beacon_client
                .publish_block(
                    unblinded_payload.clone(),
                    Some(BroadcastValidation::ConsensusAndEquivocation),
                    fork,
                )
                .await
            {
                error!(request_id = %request_id, error = %err, "error publishing block");
                return Err(err.into())
            }

            trace.beacon_client_broadcast = get_nanos_timestamp()?;

            // Broadcast payload to all broadcasters
            self.broadcast_signed_block(
                unblinded_payload.clone(),
                Some(BroadcastValidation::Gossip),
                request_id,
            );
            trace.broadcaster_block_broadcast = get_nanos_timestamp()?;

            // While we wait for the block to propagate, we also store the payload information
            trace.on_deliver_payload = get_nanos_timestamp()?;
            self.save_delivered_payload_info(
                payload.clone(),
                &signed_blinded_block,
                &proposer_public_key,
                trace,
                request_id,
            )
            .await;

            // Calculate the remaining time needed to reach the target propagation duration.
            // Conditionally pause the execution until we hit
            // `TARGET_GET_PAYLOAD_PROPAGATION_DURATION_MS` to allow the block to
            // propagate through the network.
            let elapsed_since_propagate_start_ms =
                (get_nanos_timestamp()?.saturating_sub(trace.beacon_client_broadcast)) / 1_000_000;
            let remaining_sleep_ms = self
                .target_get_payload_propagation_duration_ms
                .saturating_sub(elapsed_since_propagate_start_ms);
            if remaining_sleep_ms > 0 {
                sleep(Duration::from_millis(remaining_sleep_ms)).await;
            }
        }

        let get_payload_response = match GetPayloadResponse::try_from_execution_payload(&payload) {
            Some(get_payload_response) => get_payload_response,
            None => {
                error!(
                    request_id = %request_id,
                    "payload type mismatch getting payload response from execution payload.
                    All previous validation steps have passed, this should not happen",
                );
                return Err(ProposerApiError::PayloadTypeMismatch)
            }
        };

        // Return response
        info!(request_id = %request_id, trace = ?trace, timestamp = get_nanos_timestamp()?, "delivering payload");
        Ok(get_payload_response)
    }
}

// HELPERS
impl<A, DB, M, G> ProposerApi<A, DB, M, G>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
    M: MultiBeaconClientTrait + 'static,
    G: GossipClientTrait + 'static,
{
    /// Validate a single registration.
    pub fn validate_registration(
        &self,
        registration: &mut SignedValidatorRegistration,
    ) -> Result<(), ProposerApiError> {
        // Validate registration time
        self.validate_registration_time(registration)?;

        // Verify the signature
        let message = &mut registration.message;
        let public_key = &message.public_key.clone();
        if let Err(err) = verify_signed_builder_message(
            message,
            &registration.signature,
            public_key,
            &self.chain_info.context,
        ) {
            return Err(ProposerApiError::InvalidSignature(err))
        }

        Ok(())
    }

    /// Validates the timestamp in a `SignedValidatorRegistration` message.
    ///
    /// - Ensures the timestamp is not too early (before genesis time)
    /// - Ensures the timestamp is not too far in the future (current time + 10 seconds).
    fn validate_registration_time(
        &self,
        registration: &SignedValidatorRegistration,
    ) -> Result<(), ProposerApiError> {
        let registration_timestamp = registration.message.timestamp as i64;
        let registration_timestamp_upper_bound =
            (get_current_unix_time_in_nanos() / 1_000_000_000) as i64 + 10;

        if registration_timestamp < self.chain_info.genesis_time_in_secs as i64 {
            return Err(ProposerApiError::TimestampTooEarly {
                timestamp: registration_timestamp as u64,
                min_timestamp: self.chain_info.genesis_time_in_secs,
            })
        } else if registration_timestamp > registration_timestamp_upper_bound {
            return Err(ProposerApiError::TimestampTooFarInTheFuture {
                timestamp: registration_timestamp as u64,
                max_timestamp: registration_timestamp_upper_bound as u64,
            })
        }

        Ok(())
    }

    /// Validates that the bid request is not sent too late within the current slot.
    ///
    /// - Only allows requests for the current slot until a certain cutoff time.
    ///
    /// Returns how many ms we are into the slot if ok.
    fn validate_bid_request_time(&self, bid_request: &BidRequest) -> Result<u64, ProposerApiError> {
        let curr_timestamp_ms = get_millis_timestamp()? as i64;
        let slot_start_timestamp = self.chain_info.genesis_time_in_secs +
            (bid_request.slot * self.chain_info.seconds_per_slot);
        let ms_into_slot = curr_timestamp_ms.saturating_sub((slot_start_timestamp * 1000) as i64);

        if ms_into_slot > GET_HEADER_REQUEST_CUTOFF_MS {
            warn!(curr_timestamp_ms = curr_timestamp_ms, slot = bid_request.slot, "get_request");

            return Err(ProposerApiError::GetHeaderRequestTooLate {
                ms_into_slot: ms_into_slot as u64,
                cutoff: GET_HEADER_REQUEST_CUTOFF_MS as u64,
            })
        }

        Ok(ms_into_slot.max(0) as u64)
    }

    /// Validates the proposal coordinate of a given `SignedBlindedBeaconBlock`.
    ///
    /// - Compares the proposer index of the block with the expected index for the current slot.
    /// - Compares the api `head_slot` with the `slot_duty` slot.
    /// - Compares the `slot_duty.slot` with the signed blinded block slot.
    async fn validate_proposal_coordinate(
        &self,
        signed_blinded_block: &SignedBlindedBeaconBlock,
        slot_duty: &BuilderGetValidatorsResponseEntry,
        head_slot: u64,
    ) -> Result<(), ProposerApiError> {
        let actual_index = signed_blinded_block.message().proposer_index();
        let expected_index = slot_duty.validator_index;

        if expected_index != actual_index {
            return Err(ProposerApiError::UnexpectedProposerIndex {
                expected: expected_index,
                actual: actual_index,
            })
        }

        if head_slot + 1 != slot_duty.slot {
            return Err(ProposerApiError::InternalSlotMismatchesWithSlotDuty {
                internal_slot: head_slot,
                slot_duty_slot: slot_duty.slot,
            })
        }

        if slot_duty.slot != signed_blinded_block.message().slot() {
            return Err(ProposerApiError::InvalidBlindedBlockSlot {
                internal_slot: slot_duty.slot,
                blinded_block_slot: signed_blinded_block.message().slot(),
            })
        }

        Ok(())
    }

    /// Validates that the `ExecutionPayloadHeader` of a given `SignedBlindedBeaconBlock` matches
    /// the known `ExecutionPayload`.
    ///
    /// - Checks the fork versions match.
    /// - Checks the equality of the local and provided header.
    /// - Returns `Ok(())` if the `ExecutionPayloadHeader` matches.
    /// - Returns `Err(ProposerApiError)` for mismatching or invalid headers.
    fn validate_header_equality(
        &self,
        local_header: &ExecutionPayloadHeader,
        provided_header: ExecutionPayloadHeaderRef<'_>,
    ) -> Result<(), ProposerApiError> {
        match local_header {
            ExecutionPayloadHeader::Bellatrix(local_header) => {
                let provided_header =
                    provided_header.bellatrix().ok_or(ProposerApiError::PayloadTypeMismatch)?;
                if local_header != provided_header {
                    return Err(ProposerApiError::BlindedBlockAndPayloadHeaderMismatch)
                }
            }
            ExecutionPayloadHeader::Capella(local_header) => {
                let provided_header =
                    provided_header.capella().ok_or(ProposerApiError::PayloadTypeMismatch)?;
                if local_header != provided_header {
                    return Err(ProposerApiError::BlindedBlockAndPayloadHeaderMismatch)
                }
            }
            ExecutionPayloadHeader::Deneb(local_header) => {
                let provided_header =
                    provided_header.deneb().ok_or(ProposerApiError::PayloadTypeMismatch)?;
                if local_header != provided_header {
                    return Err(ProposerApiError::BlindedBlockAndPayloadHeaderMismatch)
                }
            }
        }
        Ok(())
    }

    fn verify_signed_blinded_block_signature(
        &self,
        signed_blinded_beacon_block: &mut SignedBlindedBeaconBlock,
        public_key: &BlsPublicKey,
        genesis_validators_root: Root,
        context: &Context,
    ) -> Result<(), ethereum_consensus::Error> {
        let slot = signed_blinded_beacon_block.message().slot();
        match signed_blinded_beacon_block {
            SignedBlindedBeaconBlock::Bellatrix(block) => verify_signed_consensus_message(
                &mut block.message,
                &block.signature,
                public_key,
                context,
                Some(slot),
                Some(genesis_validators_root),
            ),
            SignedBlindedBeaconBlock::Capella(block) => verify_signed_consensus_message(
                &mut block.message,
                &block.signature,
                public_key,
                context,
                Some(slot),
                Some(genesis_validators_root),
            ),
            SignedBlindedBeaconBlock::Deneb(block) => verify_signed_consensus_message(
                &mut block.message,
                &block.signature,
                public_key,
                context,
                Some(slot),
                Some(genesis_validators_root),
            ),
        }
    }

    /// `broadcast_signed_block` sends the provided signed block to all registered broadcasters
    /// (e.g., BloXroute, Fiber).
    fn broadcast_signed_block(
        &self,
        signed_block: Arc<VersionedSignedProposal>,
        broadcast_validation: Option<BroadcastValidation>,
        request_id: &Uuid,
    ) {
        info!("broadcasting signed block");

        if self.broadcasters.is_empty() {
            warn!("no broadcasters registered");
            return
        }

        for broadcaster in self.broadcasters.iter() {
            let broadcaster = broadcaster.clone();
            let block = signed_block.clone();
            let broadcast_validation = broadcast_validation.clone();
            let consensus_version = get_consensus_version(block.beacon_block());
            let request_id = *request_id;
            tokio::spawn(async move {
                info!(request_id = %request_id, broadcaster = %broadcaster.identifier(), "broadcast_signed_block");

                if let Err(err) = broadcaster
                    .broadcast_block(block, broadcast_validation, consensus_version)
                    .await
                {
                    warn!(
                        request_id = %request_id,
                        broadcaster = broadcaster.identifier(),
                        error = %err,
                        "error broadcasting signed block",
                    );
                }
            });
        }
    }

    /// If there are blobs in the unblinded payload, this function will send them directly to the
    /// beacon chain to be propagated async to the full block.
    async fn gossip_blobs(
        &self,
        unblinded_payload: Arc<VersionedSignedProposal>,
        request_id: Uuid,
    ) {
        let blob_sidecars = match BlobSidecars::try_from_unblinded_payload(
            unblinded_payload.clone(),
        ) {
            Ok(blob_sidecars) => blob_sidecars,
            Err(err) => {
                match err {
                    BuildBlobSidecarError::NoBlobsInPayload |
                    BuildBlobSidecarError::PayloadVersionBeforeBlobs => {}
                    error => {
                        error!(%request_id, ?error, "gossip blobs: failed to build blob sidecars for async gossiping");
                    }
                }
                return
            }
        };

        info!(%request_id, "gossip blobs: successfully built blob sidecars for request. Gossiping async..");

        // Send blob sidecars to beacon clients.
        let publish_blob_request = PublishBlobsRequest {
            blob_sidecars,
            beacon_root: unblinded_payload.beacon_block().message().parent_root(),
        };
        if let Err(error) = self.multi_beacon_client.publish_blobs(publish_blob_request).await {
            error!(%request_id, ?error, "gossip blobs: failed to gossip blob sidecars");
        }
    }

    /// This function should be run as a seperate async task.
    /// Will process new gossiped messages from
    async fn process_gossiped_info(&self, mut recveiver: Receiver<GossipedMessage>) {
        while let Some(msg) = recveiver.recv().await {
            if let GossipedMessage::GetPayload(payload) = msg {
                let api_clone = self.clone();
                tokio::spawn(async move {
                    let mut trace = GetPayloadTrace {
                        receive: get_nanos_timestamp().unwrap_or_default(),
                        ..Default::default()
                    };
                    info!(request_id = %payload.request_id, "processing gossiped payload");
                    match api_clone
                        ._get_payload(
                            payload.signed_blinded_beacon_block,
                            &mut trace,
                            &payload.request_id,
                        )
                        .await
                    {
                        Ok(_get_payload_response) => {
                            info!(request_id = %payload.request_id, "gossiped payload processed");
                        }
                        Err(err) => {
                            error!(request_id = %payload.request_id, error = %err, "error processing gossiped payload");
                        }
                    }
                });
            }
        }
    }

    /// Fetches the execution payload associated with a given slot, public key, and block hash.
    ///
    /// The function will retry until the slot cutoff is reached.
    async fn get_execution_payload(
        &self,
        slot: u64,
        pub_key: &BlsPublicKey,
        block_hash: &ByteVector<32>,
        request_id: &Uuid,
    ) -> Result<PayloadAndBlobs, ProposerApiError> {
        const RETRY_DELAY: Duration = Duration::from_millis(20);

        let slot_time =
            self.chain_info.genesis_time_in_secs + (slot * self.chain_info.seconds_per_slot);
        let slot_cutoff_millis = (slot_time * 1000) + GET_PAYLOAD_REQUEST_CUTOFF_MS as u64;

        let mut last_error: Option<ProposerApiError> = None;
        let mut first_try = true; // Try at least once to cover case where get_payload is called too late.
        while first_try || get_millis_timestamp()? < slot_cutoff_millis {
            match self.auctioneer.get_execution_payload(slot, pub_key, block_hash).await {
                Ok(Some(versioned_payload)) => return Ok(versioned_payload),
                Ok(None) => {
                    warn!(request_id = %request_id, "execution payload not found");
                }
                Err(err) => {
                    error!(request_id = %request_id, error = %err, "error fetching execution payload");
                    last_error = Some(ProposerApiError::AuctioneerError(err));
                }
            }

            first_try = false;
            sleep(RETRY_DELAY).await;
        }

        error!(request_id = %request_id, "max retries reached trying to fetch execution payload");
        Err(last_error.unwrap_or_else(|| ProposerApiError::NoExecutionPayloadFound))
    }

    async fn await_and_validate_slot_start_time(
        &self,
        signed_blinded_block: &SignedBlindedBeaconBlock,
        request_time: u64,
        request_id: &Uuid,
    ) -> Result<(), ProposerApiError> {
        let (ms_into_slot, duration_until_slot_start) = calculate_slot_time_info(
            &self.chain_info,
            signed_blinded_block.message().slot(),
            request_time,
        );

        if duration_until_slot_start.as_millis() > 0 {
            info!(request_id = %request_id, "waiting until slot start t=0: {} ms", duration_until_slot_start.as_millis());
            sleep(duration_until_slot_start).await;
        } else if ms_into_slot > GET_PAYLOAD_REQUEST_CUTOFF_MS {
            return Err(ProposerApiError::GetPayloadRequestTooLate {
                cutoff: GET_PAYLOAD_REQUEST_CUTOFF_MS as u64,
                request_time: ms_into_slot as u64,
            })
        }
        Ok(())
    }

    async fn save_delivered_payload_info(
        &self,
        payload: Arc<PayloadAndBlobs>,
        signed_blinded_block: &SignedBlindedBeaconBlock,
        proposer_public_key: &BlsPublicKey,
        trace: &GetPayloadTrace,
        request_id: &Uuid,
    ) {
        let bid_trace = match self
            .auctioneer
            .get_bid_trace(
                signed_blinded_block.message().slot(),
                proposer_public_key,
                payload.execution_payload.block_hash(),
            )
            .await
        {
            Ok(Some(bt)) => bt,
            Ok(None) => {
                error!(request_id = %request_id, "bid trace not found");
                return
            }
            Err(err) => {
                error!(request_id = %request_id, error = %err, "error fetching bid trace from auctioneer");
                return
            }
        };

        let db = self.db.clone();
        let trace = trace.clone();
        let request_id = *request_id;
        tokio::spawn(async move {
            if let Err(err) = db.save_delivered_payload(&bid_trace, payload, &trace).await {
                error!(request_id = %request_id, error = %err, "error saving payload to database");
            }
        });
    }

    async fn save_get_header_call(
        &self,
        slot: u64,
        parent_hash: ByteVector<32>,
        public_key: BlsPublicKey,
        best_block_hash: ByteVector<32>,
        trace: GetHeaderTrace,
        request_id: Uuid,
        user_agent: Option<String>,
    ) {
        let db = self.db.clone();

        tokio::spawn(async move {
            if let Err(err) = db
                .save_get_header_call(
                    slot,
                    parent_hash,
                    public_key,
                    best_block_hash,
                    trace,
                    user_agent,
                )
                .await
            {
                error!(request_id = %request_id, error = %err, "error saving get header call to database");
            }
        });
    }

    async fn is_trusted_proposer(
        &self,
        public_key: &BlsPublicKey,
    ) -> Result<bool, ProposerApiError> {
        let is_trusted_proposer = self.auctioneer.is_trusted_proposer(public_key).await?;
        Ok(is_trusted_proposer)
    }
}

async fn deserialize_get_payload_bytes(
    req: Request<Body>,
) -> Result<SignedBlindedBeaconBlock, ProposerApiError> {
    let body = req.into_body();
    let body_bytes = to_bytes(body, MAX_BLINDED_BLOCK_LENGTH).await?;
    Ok(serde_json::from_slice(&body_bytes)?)
}

// STATE SYNC
impl<A, DB, M, G> ProposerApi<A, DB, M, G>
where
    A: Auctioneer,
    DB: DatabaseService,
    M: MultiBeaconClientTrait,
    G: GossipClientTrait + 'static,
{
    /// Subscribes to slot head updater.
    /// Updates the current slot and next proposer duty.
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
                ChainUpdate::PayloadAttributesUpdate(_) => {}
            }
        }

        Ok(())
    }

    /// Handle a new slot update.
    /// Updates the next proposer duty for the new slot.
    async fn handle_new_slot(&self, slot_update: SlotUpdate) {
        let epoch = slot_update.slot / SLOTS_PER_EPOCH;
        info!(
            epoch = epoch,
            slot = slot_update.slot,
            slot_start_next_epoch = (epoch + 1) * SLOTS_PER_EPOCH,
            next_proposer_duty = ?slot_update.next_duty,
            "Updated head slot",
        );

        *self.curr_slot_info.write().await = (slot_update.slot, slot_update.next_duty);
    }
}

/// Calculates the time information for a given slot.
fn calculate_slot_time_info(
    chain_info: &ChainInfo,
    slot: u64,
    request_time: u64,
) -> (i64, Duration) {
    let slot_start_timestamp_in_secs =
        chain_info.genesis_time_in_secs + (slot * chain_info.seconds_per_slot);
    let ms_into_slot =
        (request_time / 1_000_000) as i64 - (slot_start_timestamp_in_secs * 1000) as i64;
    let duration_until_slot_start = chain_info.clock.duration_until_slot(slot);

    (ms_into_slot, duration_until_slot_start)
}

pub fn get_nanos_timestamp() -> Result<u64, ProposerApiError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .map_err(|_| ProposerApiError::InternalServerError)
}

fn get_millis_timestamp() -> Result<u64, ProposerApiError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|_| ProposerApiError::InternalServerError)
}

fn get_consensus_version(block: &SignedBeaconBlock) -> ethereum_consensus::Fork {
    match block {
        SignedBeaconBlock::Phase0(_) => ethereum_consensus::Fork::Phase0,
        SignedBeaconBlock::Altair(_) => ethereum_consensus::Fork::Altair,
        SignedBeaconBlock::Bellatrix(_) => ethereum_consensus::Fork::Bellatrix,
        SignedBeaconBlock::Capella(_) => ethereum_consensus::Fork::Capella,
        SignedBeaconBlock::Deneb(_) => ethereum_consensus::Fork::Deneb,
    }
}
