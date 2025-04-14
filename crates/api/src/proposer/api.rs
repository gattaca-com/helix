use std::{sync::Arc, time::Duration};

use alloy_primitives::{B256, U256};
use axum::{
    body::{to_bytes, Body},
    extract::{Json, Path},
    http::{HeaderMap, Request, StatusCode},
    response::IntoResponse,
    Extension,
};
use helix_beacon::{types::BroadcastValidation, BlockBroadcaster, MultiBeaconClientTrait};
use helix_common::{
    api::{
        builder_api::BuilderGetValidatorsResponseEntry, proposer_api::ValidatorRegistrationInfo,
    },
    beacon_api::PublishBlobsRequest,
    bid_submission::v3::header_submission_v3::PayloadSocketAddress,
    blob_sidecars::blob_sidecars_from_unblinded_payload,
    chain_info::{ChainInfo, Network},
    metrics::{GetHeaderMetric, PROPOSER_GOSSIP_QUEUE},
    task, BidRequest, Filtering, GetHeaderTrace, GetPayloadTrace, RegisterValidatorsTrace,
    RelayConfig, ValidatorPreferences,
};
use helix_database::DatabaseService;
use helix_datastore::{error::AuctioneerError, Auctioneer};
use helix_housekeeper::{ChainUpdate, SlotUpdate};
use helix_types::{
    BlsPublicKey, ChainSpec, ExecPayload, ExecutionPayloadHeader, GetPayloadResponse,
    PayloadAndBlobs, SigError, SignedBlindedBeaconBlock, SignedValidatorRegistration, Slot,
    SlotClockTrait, VersionedSignedProposal,
};
use helix_utils::{extract_request_id, utcnow_ms, utcnow_ns, utcnow_sec};
use serde_json::json;
use tokio::{
    sync::{
        mpsc::{self, error::SendError, Receiver, Sender},
        oneshot, RwLock,
    },
    time::{sleep, Instant},
};
use tracing::{debug, error, info, trace, warn, Instrument};
use uuid::Uuid;

use crate::{
    gossiper::{
        traits::GossipClientTrait,
        types::{
            BroadcastGetPayloadParams, BroadcastPayloadParams, GossipedMessage,
            RequestPayloadParams,
        },
    },
    proposer::{
        error::ProposerApiError, unblind_beacon_block, GetHeaderParams, PreferencesHeader,
        GET_HEADER_REQUEST_CUTOFF_MS,
    },
};

const GET_PAYLOAD_REQUEST_CUTOFF_MS: i64 = 4000;
pub(crate) const MAX_BLINDED_BLOCK_LENGTH: usize = 1024 * 1024;
pub(crate) const _MAX_VAL_REGISTRATIONS_LENGTH: usize = 425 * 10_000; // 425 bytes per registration (json) * 10,000 registrations

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

    relay_config: RelayConfig,

    /// Channel on which to send v3 payload fetch requests.
    v3_payload_request: Sender<(B256, PayloadSocketAddress)>,
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
        gossip_receiver: Receiver<GossipedMessage>,
        relay_config: RelayConfig,
        v3_payload_request: Sender<(B256, PayloadSocketAddress)>,
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
            relay_config,
            v3_payload_request,
        };

        // Spin up gossip processing task
        let api_clone = api.clone();
        task::spawn(file!(), line!(), async move {
            api_clone.process_gossiped_info(gossip_receiver).await;
        });

        // Spin up the housekeep task
        let api_clone = api.clone();
        task::spawn(file!(), line!(), async move {
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
    #[tracing::instrument(skip_all, fields(id =% extract_request_id(&headers)))]
    pub async fn status(
        Extension(_proposer_api): Extension<Arc<ProposerApi<A, DB, M, G>>>,
        headers: HeaderMap,
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
    #[tracing::instrument(skip_all, fields(id =% extract_request_id(&headers)), err)]
    pub async fn register_validators(
        Extension(proposer_api): Extension<Arc<ProposerApi<A, DB, M, G>>>,
        headers: HeaderMap,
        Json(registrations): Json<Vec<SignedValidatorRegistration>>,
    ) -> Result<StatusCode, ProposerApiError> {
        if registrations.is_empty() {
            return Err(ProposerApiError::EmptyRequest);
        }

        let mut trace = RegisterValidatorsTrace { receive: utcnow_ns(), ..Default::default() };

        // Get optional api key from headers
        let api_key = headers.get("x-api-key").and_then(|key| key.to_str().ok());

        let pool_name = match api_key {
            Some(api_key) => match proposer_api.db.get_validator_pool_name(api_key).await? {
                Some(pool_name) => Some(pool_name),
                None => {
                    warn!("Invalid api key provided");
                    return Err(ProposerApiError::InvalidApiKey);
                }
            },
            None => None,
        };

        // Set using default preferences from config
        let mut validator_preferences = ValidatorPreferences {
            filtering: proposer_api.validator_preferences.filtering,
            trusted_builders: proposer_api.validator_preferences.trusted_builders.clone(),
            header_delay: proposer_api.validator_preferences.header_delay,
            delay_ms: proposer_api.validator_preferences.delay_ms,
            gossip_blobs: proposer_api.validator_preferences.gossip_blobs,
        };

        let preferences_header = headers.get("x-preferences");

        debug!(
            pool_name = ?pool_name,
            preferences_header = ?preferences_header,
        );

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

        let user_agent =
            headers.get("user-agent").and_then(|v| v.to_str().ok()).map(|v| v.to_string());

        let (head_slot, _) = *proposer_api.curr_slot_info.read().await;
        let num_registrations = registrations.len();
        trace!(head_slot = head_slot, num_registrations = num_registrations,);

        // Bulk check if the validators are known
        let registration_pub_keys =
            registrations.iter().map(|r| r.message.pubkey.clone()).collect();
        let known_pub_keys = proposer_api.db.check_known_validators(registration_pub_keys).await?;

        // Check each registration
        let mut valid_registrations = Vec::with_capacity(known_pub_keys.len());

        let mut handles = Vec::with_capacity(registrations.len());

        for registration in registrations {
            let proposer_api_clone = proposer_api.clone();
            let start_time = Instant::now();

            let pub_key = registration.message.pubkey.clone();

            trace!(
                pub_key = ?pub_key,
                fee_recipient = %registration.message.fee_recipient,
                gas_limit = registration.message.gas_limit,
                timestamp = registration.message.timestamp,
            );

            if !known_pub_keys.contains(&pub_key) {
                warn!(
                    pub_key = ?pub_key,
                    "Registration for unknown validator",
                );
                continue;
            }

            if !proposer_api_clone.db.is_registration_update_required(&registration).await? {
                trace!(
                    pub_key = ?pub_key,
                    "Registration update not required",
                );
                valid_registrations.push(registration);
                continue;
            }

            let handle = tokio::task::spawn_blocking(move || {
                let res = match proposer_api_clone.validate_registration(&registration) {
                    Ok(_) => Some(registration),
                    Err(err) => {
                        warn!(%err, ?pub_key, "Failed to register validator");
                        None
                    }
                };

                trace!(?pub_key, elapsed_time = %start_time.elapsed().as_nanos(),);

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

        let successful_registrations = valid_registrations.len();

        // Bulk write registrations to db
        task::spawn(file!(), line!(), async move {
            // Add validator preferences to each registration
            let mut valid_registrations_infos = Vec::new();

            for reg in valid_registrations {
                let preferences = validator_preferences.clone();
                valid_registrations_infos
                    .push(ValidatorRegistrationInfo { registration: reg, preferences });
            }

            if let Err(err) = proposer_api
                .db
                .save_validator_registrations(valid_registrations_infos, pool_name, user_agent)
                .await
            {
                error!(
                    %err,
                    "failed to save validator registrations",
                );
            }
        });

        trace.registrations_complete = utcnow_ns();

        debug!(
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
    #[tracing::instrument(skip_all, fields(id =% extract_request_id(&headers)), err)]
    pub async fn get_header(
        Extension(proposer_api): Extension<Arc<ProposerApi<A, DB, M, G>>>,
        headers: HeaderMap,
        Path(GetHeaderParams { slot, parent_hash, public_key }): Path<GetHeaderParams>,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        if proposer_api.auctioneer.kill_switch_enabled().await? {
            return Err(ProposerApiError::ServiceUnavailableError);
        }

        let mut trace = GetHeaderTrace { receive: utcnow_ns(), ..Default::default() };

        let (head_slot, duty) = proposer_api.curr_slot_info.read().await.clone();
        debug!(
            head_slot = head_slot,
            request_ts = trace.receive,
            slot = slot,
            parent_hash = ?parent_hash,
            public_key = ?public_key,
        );

        let bid_request = BidRequest { slot: slot.into(), parent_hash, public_key };

        // Dont allow requests for past slots
        if bid_request.slot < head_slot {
            debug!("request for past slot");
            return Err(ProposerApiError::RequestForPastSlot {
                request_slot: bid_request.slot.into(),
                head_slot,
            });
        }

        // Only return a bid if there is a proposer connected this slot.
        if duty.is_none() {
            debug!("proposer duty not found");
            return Err(ProposerApiError::ProposerNotRegistered);
        }
        let duty = duty.unwrap();

        let ms_into_slot = match proposer_api.validate_bid_request_time(&bid_request) {
            Ok(ms_into_slot) => ms_into_slot,
            Err(err) => {
                warn!(%err, "invalid bid request time");
                return Err(err);
            }
        };
        trace.validation_complete = utcnow_ns();

        let user_agent =
            headers.get("user-agent").and_then(|v| v.to_str().ok()).map(|v| v.to_string());

        let mut mev_boost = false;

        if let Some(request_initiated_ms) = get_x_mev_boost_header_start_ms(&headers) {
            let latency = utcnow_ms().saturating_sub(request_initiated_ms);
            debug!(%request_initiated_ms, %latency, "mev-boost start ts header found");
            mev_boost = true;
        }

        // If timing games are enabled for the proposer then we sleep a fixed amount
        // TODO: remove is trusted proposer once people start using header verification

        if duty.entry.preferences.header_delay {
            let latest_header_delay_ms_in_slot =
                proposer_api.relay_config.timing_game_config.latest_header_delay_ms_in_slot;

            let max_header_delay_ms =
                proposer_api.relay_config.timing_game_config.max_header_delay_ms;
            let proposer_delay = duty.entry.preferences.delay_ms.unwrap_or(max_header_delay_ms);

            let sleep_time_ms = std::cmp::min(
                latest_header_delay_ms_in_slot.saturating_sub(ms_into_slot),
                proposer_delay,
            );

            let sleep_time = Duration::from_millis(sleep_time_ms);
            let mut get_header_metric = GetHeaderMetric::new(sleep_time);

            debug!(target: "timing_games", 
                ?sleep_time,
                %ms_into_slot,
                %proposer_delay,
                max_header_delay_ms,
                latest_header_delay_ms_in_slot,
                slot,
                pubkey = ?bid_request.public_key,
                "timing game sleep");

            if sleep_time > Duration::ZERO {
                sleep(sleep_time).await;
            }

            get_header_metric.record();
        }

        // Get best bid from auctioneer
        let get_best_bid_res = proposer_api
            .auctioneer
            .get_best_bid(
                bid_request.slot.into(),
                &bid_request.parent_hash,
                &bid_request.public_key,
            )
            .await;
        trace.best_bid_fetched = utcnow_ns();
        debug!(trace = ?trace, "best bid fetched");

        match get_best_bid_res {
            Ok(Some(bid)) => {
                if bid.data.message.value() == &U256::ZERO {
                    warn!("best bid value is 0");
                    return Err(ProposerApiError::BidValueZero);
                }

                debug!(
                    value = ?bid.data.message.value(),
                    block_hash = ?bid.data.message.header().block_hash(),
                    "delivering bid",
                );

                // Save trace to DB
                proposer_api
                    .save_get_header_call(
                        slot,
                        bid_request.parent_hash,
                        bid_request.public_key.clone(),
                        bid.data.message.header().block_hash().0,
                        trace,
                        mev_boost,
                        user_agent.clone(),
                    )
                    .await;

                let proposer_pubkey_clone = bid_request.public_key;
                let block_hash = bid.data.message.header().block_hash().0;
                if user_agent.is_some() && is_mev_boost_client(&user_agent.unwrap()) {
                    // Request payload in the background
                    task::spawn(file!(), line!(), async move {
                        if let Err(err) = proposer_api
                            .gossiper
                            .request_payload(RequestPayloadParams {
                                slot,
                                proposer_pub_key: proposer_pubkey_clone,
                                block_hash,
                            })
                            .await
                        {
                            error!(%err, "failed to request payload");
                        }
                    });
                }

                info!(bid = %json!(bid), "delivering bid");

                // Return header
                Ok(axum::Json(bid))
            }
            Ok(None) => {
                warn!("no bid found");
                Err(ProposerApiError::NoBidPrepared)
            }
            Err(err) => {
                error!(%err, "error getting bid");
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
    #[tracing::instrument(skip_all, fields(id))]
    pub async fn get_payload(
        Extension(proposer_api): Extension<Arc<ProposerApi<A, DB, M, G>>>,
        headers: HeaderMap,
        req: Request<Body>,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        let request_id = extract_request_id(&headers);
        tracing::Span::current().record("id", request_id.to_string());

        let mut trace = GetPayloadTrace { receive: utcnow_ns(), ..Default::default() };

        let user_agent =
            headers.get("user-agent").and_then(|v| v.to_str().ok()).map(|v| v.to_string());

        let signed_blinded_block: SignedBlindedBeaconBlock =
            match deserialize_get_payload_bytes(req).await {
                Ok(signed_block) => signed_block,
                Err(err) => {
                    warn!(
                        error = %err,
                        "failed to deserialize signed block",
                    );
                    return Err(err);
                }
            };

        let block_hash = signed_blinded_block
            .message()
            .body()
            .execution_payload()
            .map_err(|_| ProposerApiError::InvalidFork)? // this should never happen as post altair there's always an execution payload
            .block_hash()
            .0;

        let slot = signed_blinded_block.message().slot();

        // Broadcast get payload request
        if let Err(err) = proposer_api
            .gossiper
            .broadcast_get_payload(BroadcastGetPayloadParams {
                signed_blinded_beacon_block: signed_blinded_block.clone(),
                request_id,
            })
            .await
        {
            error!(%err, "failed to broadcast get payload");
        };

        match proposer_api._get_payload(signed_blinded_block, &mut trace, user_agent).await {
            Ok(get_payload_response) => Ok(axum::Json(get_payload_response)),
            Err(err) => {
                // Save error to DB
                if let Err(err) = proposer_api
                    .db
                    .save_failed_get_payload(slot.into(), block_hash, err.to_string(), trace)
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
        user_agent: Option<String>,
    ) -> Result<GetPayloadResponse, ProposerApiError> {
        let block_hash = signed_blinded_block
            .message()
            .body()
            .execution_payload()
            .map_err(|_| ProposerApiError::InvalidFork)?
            .block_hash()
            .0;

        let (head_slot, slot_duty) = self.curr_slot_info.read().await.clone();

        info!(head_slot, request_ts = trace.receive, ?block_hash,);

        // Verify that the request is for the current slot
        if signed_blinded_block.message().slot() <= head_slot {
            warn!("request for past slot");
            return Err(ProposerApiError::RequestForPastSlot {
                request_slot: signed_blinded_block.message().slot().into(),
                head_slot,
            });
        }

        // Verify that we have a proposer connected for the current proposal
        if slot_duty.is_none() {
            warn!("no slot proposer duty");
            return Err(ProposerApiError::ProposerNotRegistered);
        }
        let slot_duty = slot_duty.unwrap();

        if let Err(err) =
            self.validate_proposal_coordinate(&signed_blinded_block, &slot_duty, head_slot).await
        {
            warn!(%err, "invalid proposal coordinate");
            return Err(err);
        }
        trace.proposer_index_validated = utcnow_ns();

        let proposer_public_key = slot_duty.entry.registration.message.pubkey;
        if let Err(err) = self.verify_signed_blinded_block_signature(
            &mut signed_blinded_block,
            &proposer_public_key,
            self.chain_info.genesis_validators_root,
            &self.chain_info.context,
        ) {
            warn!(%err, "invalid signature");
            return Err(ProposerApiError::InvalidSignature);
        }
        trace.signature_validated = utcnow_ns();

        if let Ok(Some(payload_address)) = self.auctioneer.get_payload_address(&block_hash).await {
            // Fetch v3 optimistic payload from builder. This will complete asynchronously.
            let _ = self.v3_payload_request.send((block_hash, payload_address)).await;
        }

        // Get execution payload from auctioneer
        let payload_result = self
            .get_execution_payload(
                signed_blinded_block.message().slot().into(),
                &proposer_public_key,
                &block_hash,
                true,
            )
            .await;

        let versioned_payload = match payload_result {
            Ok(p) => p,
            Err(err) => {
                error!(
                    slot = %signed_blinded_block.message().slot(),
                    proposer_public_key = ?proposer_public_key,
                    block_hash = ?block_hash,
                    %err,
                    "No payload found for slot"
                );
                return Err(ProposerApiError::NoExecutionPayloadFound);
            }
        };
        info!("found payload for blinded signed block");
        trace.payload_fetched = utcnow_ns();

        // Check if get_payload has already been called
        if let Err(err) = self
            .auctioneer
            .check_and_set_last_slot_and_hash_delivered(
                signed_blinded_block.message().slot().into(),
                &block_hash,
            )
            .await
        {
            match err {
                AuctioneerError::AnotherPayloadAlreadyDeliveredForSlot => {
                    warn!("validator called get_payload twice for different block hashes");
                    return Err(ProposerApiError::AuctioneerError(err));
                }
                AuctioneerError::PastSlotAlreadyDelivered => {
                    warn!("validator called get_payload for past slot");
                    return Err(ProposerApiError::AuctioneerError(err));
                }
                _ => {
                    // If error was internal carry on
                    error!(%err, "error checking and setting last slot and hash delivered");
                }
            }
        }

        // Handle early/late requests
        if let Err(err) =
            self.await_and_validate_slot_start_time(&signed_blinded_block, trace.receive).await
        {
            warn!(error = %err, "get_payload was sent too late");

            // Save too late request to db for debugging
            if let Err(err) = self
                .db
                .save_too_late_get_payload(
                    signed_blinded_block.message().slot().into(),
                    &proposer_public_key,
                    &block_hash,
                    trace.receive,
                    trace.payload_fetched,
                )
                .await
            {
                error!(%err, "failed to save too late get payload");
            }

            return Err(err);
        }

        if let Err(err) = self.validate_block_equality(&versioned_payload, &signed_blinded_block) {
            error!(
                %err,
                "execution payload invalid, does not match known ExecutionPayload",
            );
            return Err(err);
        }

        trace.validation_complete = utcnow_ns();

        // TODO: merge logic with validate_block_equality
        let unblinded_payload =
            match unblind_beacon_block(&signed_blinded_block, &versioned_payload) {
                Ok(unblinded_payload) => Arc::new(unblinded_payload),
                Err(err) => {
                    warn!(%err, "payload type mismatch in unblind block");
                    return Err(ProposerApiError::PayloadTypeMismatch);
                }
            };
        let payload = Arc::new(versioned_payload);

        if self.validator_preferences.gossip_blobs ||
            !matches!(self.chain_info.network, Network::Mainnet)
        {
            info!("gossip blobs: about to gossip blobs");
            let self_clone = self.clone();
            let unblinded_payload_clone = unblinded_payload.clone();

            task::spawn(
                file!(),
                line!(),
                async move {
                    self_clone.gossip_blobs(unblinded_payload_clone).await;
                }
                .in_current_span(),
            );
        }

        let is_trusted_proposer = self.is_trusted_proposer(&proposer_public_key).await?;

        // Publish and validate payload with multi-beacon-client
        let fork = unblinded_payload.signed_block.fork_name_unchecked();

        let (tx, rx) = oneshot::channel();

        let self_clone = self.clone();
        let unblinded_payload_clone = unblinded_payload.clone();
        let mut trace_clone = *trace;
        let payload_clone = payload.clone();

        task::spawn(file!(), line!(), async move {
            if let Err(err) = self_clone
                .multi_beacon_client
                .publish_block(
                    unblinded_payload_clone.clone(),
                    Some(BroadcastValidation::ConsensusAndEquivocation),
                    fork,
                )
                .await
            {
                error!(%err, "error publishing block");
            };

            trace_clone.beacon_client_broadcast = utcnow_ns();

            // Broadcast payload to all broadcasters
            self_clone.broadcast_signed_block(
                unblinded_payload_clone.clone(),
                Some(BroadcastValidation::Gossip),
            );
            trace_clone.broadcaster_block_broadcast = utcnow_ns();

            // While we wait for the block to propagate, we also store the payload information
            trace_clone.on_deliver_payload = utcnow_ns();
            self_clone
                .save_delivered_payload_info(
                    payload_clone,
                    &signed_blinded_block,
                    &proposer_public_key,
                    &trace_clone,
                    user_agent,
                )
                .await;

            if !is_trusted_proposer && tx.send(()).is_err() {
                error!("Error sending beacon client response, receiver dropped");
            }
        });

        if !is_trusted_proposer {
            if (rx.await).is_ok() {
                info!(?trace, "Payload published and saved!")
            } else {
                error!("Error in beacon client publishing");
                return Err(ProposerApiError::InternalServerError);
            }

            // Calculate the remaining time needed to reach the target propagation duration.
            // Conditionally pause the execution until we hit
            // `TARGET_GET_PAYLOAD_PROPAGATION_DURATION_MS` to allow the block to
            // propagate through the network.
            let elapsed_since_propagate_start_ms =
                (utcnow_ns().saturating_sub(trace.beacon_client_broadcast)) / 1_000_000;
            let remaining_sleep_ms = self
                .relay_config
                .target_get_payload_propagation_duration_ms
                .saturating_sub(elapsed_since_propagate_start_ms);
            if remaining_sleep_ms > 0 {
                sleep(Duration::from_millis(remaining_sleep_ms)).await;
            }
        }

        let get_payload_response = GetPayloadResponse::new_no_metadata(Some(fork), payload);

        // Return response
        info!(?trace, timestamp = utcnow_ns(), "delivering payload");
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
        registration: &SignedValidatorRegistration,
    ) -> Result<(), ProposerApiError> {
        // Validate registration time
        self.validate_registration_time(registration)?;

        // Verify the signature
        if !registration.verify_signature(&self.chain_info.context) {
            return Err(ProposerApiError::InvalidSignature);
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
        let registration_timestamp = registration.message.timestamp;
        let registration_timestamp_upper_bound = utcnow_sec() + 10;

        if registration_timestamp < self.chain_info.genesis_time_in_secs {
            return Err(ProposerApiError::TimestampTooEarly {
                timestamp: registration_timestamp,
                min_timestamp: self.chain_info.genesis_time_in_secs,
            });
        } else if registration_timestamp > registration_timestamp_upper_bound {
            return Err(ProposerApiError::TimestampTooFarInTheFuture {
                timestamp: registration_timestamp,
                max_timestamp: registration_timestamp_upper_bound,
            });
        }

        Ok(())
    }

    /// Validates that the bid request is not sent too late within the current slot.
    ///
    /// - Only allows requests for the current slot until a certain cutoff time.
    ///
    /// Returns how many ms we are into the slot if ok.
    fn validate_bid_request_time(&self, bid_request: &BidRequest) -> Result<u64, ProposerApiError> {
        let curr_timestamp_ms = utcnow_ms() as i64;
        let slot_start_timestamp = self.chain_info.genesis_time_in_secs +
            (bid_request.slot.as_u64() * self.chain_info.seconds_per_slot());
        let ms_into_slot = curr_timestamp_ms.saturating_sub((slot_start_timestamp * 1000) as i64);

        if ms_into_slot > GET_HEADER_REQUEST_CUTOFF_MS {
            warn!(curr_timestamp_ms = curr_timestamp_ms, slot = %bid_request.slot, "get_request");

            return Err(ProposerApiError::GetHeaderRequestTooLate {
                ms_into_slot: ms_into_slot as u64,
                cutoff: GET_HEADER_REQUEST_CUTOFF_MS as u64,
            });
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
            });
        }

        if head_slot + 1 != slot_duty.slot.as_u64() {
            return Err(ProposerApiError::InternalSlotMismatchesWithSlotDuty {
                internal_slot: head_slot,
                slot_duty_slot: slot_duty.slot.into(),
            });
        }

        if slot_duty.slot != signed_blinded_block.message().slot() {
            return Err(ProposerApiError::InvalidBlindedBlockSlot {
                internal_slot: slot_duty.slot.into(),
                blinded_block_slot: signed_blinded_block.message().slot().into(),
            });
        }

        Ok(())
    }

    /// Validates that the `SignedBlindedBeaconBlock` matches the known `ExecutionPayload`.
    ///
    /// - Checks the fork versions match.
    /// - Checks the equality of the local and provided header.
    /// - Checks the equality of the kzg commitments.
    /// - Returns `Ok(())` if the `ExecutionPayloadHeader` matches.
    /// - Returns `Err(ProposerApiError)` for mismatching or invalid headers.
    fn validate_block_equality(
        &self,
        local_versioned_payload: &PayloadAndBlobs,
        provided_signed_blinded_block: &SignedBlindedBeaconBlock,
    ) -> Result<(), ProposerApiError> {
        let message = provided_signed_blinded_block.message();
        let body = message.body();

        let local_header = local_versioned_payload.execution_payload.to_ref().into();

        match local_header {
            ExecutionPayloadHeader::Bellatrix(_) |
            ExecutionPayloadHeader::Capella(_) |
            ExecutionPayloadHeader::Fulu(_) => {
                return Err(ProposerApiError::UnsupportedBeaconChainVersion)
            }

            ExecutionPayloadHeader::Deneb(local_header) => {
                let provided_header = &body
                    .execution_payload_deneb()
                    .map_err(|_| ProposerApiError::PayloadTypeMismatch)?
                    .execution_payload_header;

                info!(
                    local_header = ?local_header,
                    provided_header = ?provided_header,
                    provided_version = %provided_signed_blinded_block.fork_name_unchecked(),
                    "validating block equality",
                );

                if &local_header != provided_header {
                    return Err(ProposerApiError::BlindedBlockAndPayloadHeaderMismatch);
                }

                let local_kzg_commitments = &local_versioned_payload.blobs_bundle.commitments;

                let provided_kzg_commitments = body
                    .blob_kzg_commitments()
                    .map_err(|_| ProposerApiError::BlobKzgCommitmentsMismatch)?;

                if local_kzg_commitments != provided_kzg_commitments {
                    return Err(ProposerApiError::BlobKzgCommitmentsMismatch);
                }
            }
            ExecutionPayloadHeader::Electra(local_header) => {
                let provided_header = &body
                    .execution_payload_electra()
                    .map_err(|_| ProposerApiError::PayloadTypeMismatch)?
                    .execution_payload_header;

                info!(
                    local_header = ?local_header,
                    provided_header = ?provided_header,
                    provided_version = %provided_signed_blinded_block.fork_name_unchecked(),
                    "validating block equality",
                );

                if &local_header != provided_header {
                    return Err(ProposerApiError::BlindedBlockAndPayloadHeaderMismatch);
                }

                let local_kzg_commitments = &local_versioned_payload.blobs_bundle.commitments;

                let provided_kzg_commitments = body
                    .blob_kzg_commitments()
                    .map_err(|_| ProposerApiError::BlobKzgCommitmentsMismatch)?;

                if local_kzg_commitments != provided_kzg_commitments {
                    return Err(ProposerApiError::BlobKzgCommitmentsMismatch);
                }
            }
        }

        Ok(())
    }

    fn verify_signed_blinded_block_signature(
        &self,
        signed_blinded_beacon_block: &mut SignedBlindedBeaconBlock,
        public_key: &BlsPublicKey,
        genesis_validators_root: B256,
        context: &ChainSpec,
    ) -> Result<(), SigError> {
        let slot = signed_blinded_beacon_block.message().slot();
        let epoch = slot.epoch(self.chain_info.slots_per_epoch());
        let fork = context.fork_at_epoch(epoch);

        debug!(%slot, %epoch, expected_fork_name = ?context.fork_name_at_epoch(epoch), message_fork_name = ?signed_blinded_beacon_block.fork_name_unchecked(), %public_key, "verifying signed blinded beacon");

        let valid = signed_blinded_beacon_block.verify_signature(
            None,
            public_key,
            &fork,
            genesis_validators_root,
            context,
        );

        if !valid {
            return Err(SigError::InvalidBlsSignature);
        }

        Ok(())
    }

    /// `broadcast_signed_block` sends the provided signed block to all registered broadcasters
    /// (e.g., BloXroute, Fiber).
    fn broadcast_signed_block(
        &self,
        signed_block: Arc<VersionedSignedProposal>,
        broadcast_validation: Option<BroadcastValidation>,
    ) {
        info!("broadcasting signed block");

        if self.broadcasters.is_empty() {
            warn!("no broadcasters registered");
            return;
        }

        for broadcaster in self.broadcasters.iter() {
            let broadcaster = broadcaster.clone();
            let block = signed_block.clone();
            let broadcast_validation = broadcast_validation.clone();

            let fork_name = block.signed_block.fork_name_unchecked();

            task::spawn(file!(), line!(), async move {
                info!(broadcaster = %broadcaster.identifier(), "broadcast_signed_block");

                if let Err(err) =
                    broadcaster.broadcast_block(block, broadcast_validation, fork_name).await
                {
                    warn!(
                        broadcaster = broadcaster.identifier(),
                        %err,
                        "error broadcasting signed block",
                    );
                }
            });
        }
    }

    /// If there are blobs in the unblinded payload, this function will send them directly to the
    /// beacon chain to be propagated async to the full block.
    async fn gossip_blobs(&self, unblinded_payload: Arc<VersionedSignedProposal>) {
        let blob_sidecars = match blob_sidecars_from_unblinded_payload(&unblinded_payload) {
            Ok(blob_sidecars) => blob_sidecars,
            Err(err) => {
                error!(?err, "gossip blobs: failed to build blob sidecars for async gossiping");
                return;
            }
        };

        info!("gossip blobs: successfully built blob sidecars for request. Gossiping async..");

        // Send blob sidecars to beacon clients.
        let publish_blob_request = PublishBlobsRequest {
            blob_sidecars,
            beacon_root: unblinded_payload.signed_block.parent_root(),
        };
        if let Err(error) = self.multi_beacon_client.publish_blobs(publish_blob_request).await {
            error!(?error, "gossip blobs: failed to gossip blob sidecars");
        }
    }

    /// This function should be run as a seperate async task.
    /// Will process new gossiped messages from
    async fn process_gossiped_info(&self, mut recveiver: Receiver<GossipedMessage>) {
        while let Some(msg) = recveiver.recv().await {
            PROPOSER_GOSSIP_QUEUE.dec();
            match msg {
                GossipedMessage::GetPayload(payload) => {
                    let api_clone = self.clone();
                    task::spawn(file!(), line!(), async move {
                        let mut trace =
                            GetPayloadTrace { receive: utcnow_ns(), ..Default::default() };
                        debug!(request_id = %payload.request_id, "processing gossiped payload");
                        match api_clone
                            ._get_payload(payload.signed_blinded_beacon_block, &mut trace, None)
                            .await
                        {
                            Ok(_get_payload_response) => {
                                debug!(request_id = %payload.request_id, "gossiped payload processed");
                            }
                            Err(err) => {
                                error!(request_id = %payload.request_id, error = %err, "error processing gossiped payload");
                            }
                        }
                    });
                }
                GossipedMessage::RequestPayload(payload) => {
                    let api_clone = self.clone();
                    task::spawn(file!(), line!(), async move {
                        let request_id = Uuid::new_v4();
                        debug!(request_id = %request_id, "processing gossiped payload request");
                        let payload_result = api_clone
                            .get_execution_payload(
                                payload.slot,
                                &payload.proposer_pub_key,
                                &payload.block_hash,
                                false,
                            )
                            .await;

                        match payload_result {
                            Ok(execution_payload) => {
                                if let Err(err) = api_clone
                                    .gossiper
                                    .broadcast_payload(BroadcastPayloadParams {
                                        execution_payload,
                                        slot: payload.slot,
                                        proposer_pub_key: payload.proposer_pub_key,
                                    })
                                    .await
                                {
                                    error!(request_id = %request_id, error = %err, "error broadcasting payload");
                                }
                            }
                            Err(err) => {
                                error!(request_id = %request_id, error = %err, "error fetching execution payload");
                            }
                        }
                    });
                }
                _ => {}
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
        block_hash: &B256,
        request_missing_payload: bool,
    ) -> Result<PayloadAndBlobs, ProposerApiError> {
        const RETRY_DELAY: Duration = Duration::from_millis(20);

        let slot_time =
            self.chain_info.genesis_time_in_secs + (slot * self.chain_info.seconds_per_slot());
        let slot_cutoff_millis = (slot_time * 1000) + GET_PAYLOAD_REQUEST_CUTOFF_MS as u64;

        let mut last_error: Option<ProposerApiError> = None;
        let mut first_try = true; // Try at least once to cover case where get_payload is called too late.
        while first_try || utcnow_ms() < slot_cutoff_millis {
            match self
                .auctioneer
                .get_execution_payload(
                    slot,
                    pub_key,
                    block_hash,
                    self.chain_info.current_fork_name(),
                )
                .await
            {
                Ok(Some(versioned_payload)) => return Ok(versioned_payload),
                Ok(None) => {
                    warn!("execution payload not found");
                    if first_try && request_missing_payload {
                        let proposer_pubkey_clone = pub_key.clone();
                        let self_clone = self.clone();
                        let block_hash = *block_hash;

                        task::spawn(
                            file!(),
                            line!(),
                            async move {
                                if let Err(err) = self_clone
                                    .gossiper
                                    .request_payload(RequestPayloadParams {
                                        slot,
                                        proposer_pub_key: proposer_pubkey_clone,
                                        block_hash,
                                    })
                                    .await
                                {
                                    error!(%err, "failed to request payload");
                                }
                            }
                            .in_current_span(),
                        );
                    }
                }
                Err(err) => {
                    error!(%err, "error fetching execution payload");
                    last_error = Some(ProposerApiError::AuctioneerError(err));
                }
            }

            first_try = false;
            sleep(RETRY_DELAY).await;
        }

        error!("max retries reached trying to fetch execution payload");
        Err(last_error.unwrap_or_else(|| ProposerApiError::NoExecutionPayloadFound))
    }

    async fn await_and_validate_slot_start_time(
        &self,
        signed_blinded_block: &SignedBlindedBeaconBlock,
        request_time_ns: u64,
    ) -> Result<(), ProposerApiError> {
        let Some((since_slot_start, until_slot_start)) = calculate_slot_time_info(
            &self.chain_info,
            signed_blinded_block.message().slot(),
            request_time_ns,
        ) else {
            error!(
                request_time_ns,
                slot = %signed_blinded_block.message().slot(),
                "slot time info not found"
            );
            return Err(ProposerApiError::SlotTooNew);
        };

        if let Some(until_slot_start) = until_slot_start {
            info!("waiting until slot start t=0: {} ms", until_slot_start.as_millis());
            sleep(until_slot_start).await;
        } else if let Some(since_slot_start) = since_slot_start {
            if since_slot_start.as_millis() > GET_PAYLOAD_REQUEST_CUTOFF_MS as u128 {
                return Err(ProposerApiError::GetPayloadRequestTooLate {
                    cutoff: GET_PAYLOAD_REQUEST_CUTOFF_MS as u64,
                    request_time: since_slot_start.as_millis() as u64,
                });
            }
        }
        Ok(())
    }

    async fn save_delivered_payload_info(
        &self,
        payload: Arc<PayloadAndBlobs>,
        signed_blinded_block: &SignedBlindedBeaconBlock,
        proposer_public_key: &BlsPublicKey,
        trace: &GetPayloadTrace,
        user_agent: Option<String>,
    ) {
        let bid_trace = match self
            .auctioneer
            .get_bid_trace(
                signed_blinded_block.message().slot().into(),
                proposer_public_key,
                &payload.execution_payload.block_hash().0,
            )
            .await
        {
            Ok(Some(bt)) => bt,
            Ok(None) => {
                error!("bid trace not found");
                return;
            }
            Err(err) => {
                error!(%err, "error fetching bid trace from auctioneer");
                return;
            }
        };

        let db = self.db.clone();
        let trace = *trace;
        task::spawn(file!(), line!(), async move {
            if let Err(err) =
                db.save_delivered_payload(&bid_trace, payload, &trace, user_agent).await
            {
                error!(%err, "error saving payload to database");
            }
        });
    }

    async fn save_get_header_call(
        &self,
        slot: u64,
        parent_hash: B256,
        public_key: BlsPublicKey,
        best_block_hash: B256,
        trace: GetHeaderTrace,
        mev_boost: bool,
        user_agent: Option<String>,
    ) {
        let db = self.db.clone();

        task::spawn(
            file!(),
            line!(),
            async move {
                if let Err(err) = db
                    .save_get_header_call(
                        slot,
                        parent_hash,
                        public_key,
                        best_block_hash,
                        trace,
                        mev_boost,
                        user_agent,
                    )
                    .await
                {
                    error!(%err, "error saving get header call to database");
                }
            }
            .in_current_span(),
        );
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

/// Fetches the timestamp set by the mev-boost client when initialising the `get_header` request.
fn get_x_mev_boost_header_start_ms(header_map: &HeaderMap) -> Option<u64> {
    let start_time_str = header_map.get("X-MEVBoost-StartTimeUnixMS")?.to_str().ok()?;
    let start_time_ms: u64 = start_time_str.parse().ok()?;
    Some(start_time_ms)
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
    async fn handle_new_slot(&self, slot_update: Box<SlotUpdate>) {
        let epoch = slot_update.slot / self.chain_info.seconds_per_slot();
        info!(
            epoch = epoch,
            slot = slot_update.slot,
            slot_start_next_epoch = (epoch + 1) * self.chain_info.slots_per_epoch(),
            next_proposer_duty = ?slot_update.next_duty,
            "Updated head slot",
        );

        *self.curr_slot_info.write().await = (slot_update.slot, slot_update.next_duty);
    }
}

/// Calculates the time information for a given slot.
fn calculate_slot_time_info(
    chain_info: &ChainInfo,
    slot: Slot,
    request_time_ns: u64,
) -> Option<(Option<Duration>, Option<Duration>)> {
    let until_slot_start = chain_info.clock.duration_to_slot(slot); // None if we're past slot time

    let slot_start = chain_info.clock.start_of(slot)?;
    let since_slot_start = Duration::from_nanos(request_time_ns).checked_sub(slot_start); // None if we're before slot time

    Some((since_slot_start, until_slot_start))
}

pub fn is_mev_boost_client(client_name: &str) -> bool {
    let keywords = ["Kiln", "mev-boost", "commit-boost", "Vouch"];
    keywords.iter().any(|&keyword| client_name.contains(keyword))
}
