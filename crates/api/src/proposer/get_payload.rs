use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request},
    response::IntoResponse,
    Extension,
};
use helix_beacon::types::BroadcastValidation;
use helix_common::{
    api::builder_api::BuilderGetValidatorsResponseEntry,
    chain_info::{ChainInfo, Network},
    metadata_provider::MetadataProvider,
    task,
    utils::{extract_request_id, utcnow_ms, utcnow_ns},
    GetPayloadTrace,
};
use helix_database::DatabaseService;
use helix_datastore::{error::AuctioneerError, Auctioneer};
use helix_types::{
    BlsPublicKey, ChainSpec, ExecPayload, ExecutionPayloadHeader, GetPayloadResponse,
    PayloadAndBlobs, SigError, SignedBlindedBeaconBlock, Slot, SlotClockTrait,
    VersionedSignedProposal,
};
use tokio::time::sleep;
use tracing::{debug, error, info, warn, Instrument};

use super::ProposerApi;
use crate::{
    constants::{GET_PAYLOAD_REQUEST_CUTOFF_MS, MAX_BLINDED_BLOCK_LENGTH},
    gossiper::types::{BroadcastGetPayloadParams, RequestPayloadParams},
    proposer::{error::ProposerApiError, unblind_beacon_block},
    Api,
};

impl<A: Api> ProposerApi<A> {
    /// Retrieves the execution payload for a given blinded beacon block.
    ///
    /// This function accepts a `SignedBlindedBeaconBlock` as input and performs several steps:
    /// 1. Validates the proposer index and verifies the block's signature.
    /// 2. Retrieves the corresponding execution payload from the auctioneer.
    /// 3. Validates the payload and publishes it to the multi-beacon client.
    /// 4. Optionally broadcasts the payload to `broadcasters`
    /// 5. Stores the delivered payload information to database.
    /// 6. Returns the unblinded payload to proposer.
    ///
    /// Implements this API: <https://ethereum.github.io/builder-specs/#/Builder/submitBlindedBlock>
    #[tracing::instrument(skip_all, fields(id))]
    pub async fn get_payload(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        Extension(on_receive_ns): Extension<u64>,
        headers: HeaderMap,
        req: Request<Body>,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        let request_id = extract_request_id(&headers);
        tracing::Span::current().record("id", request_id.to_string());

        let mut trace = GetPayloadTrace { receive: on_receive_ns, ..Default::default() };

        let user_agent = proposer_api.metadata_provider.get_metadata(&headers);

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
        proposer_api
            .gossiper
            .broadcast_get_payload(BroadcastGetPayloadParams {
                signed_blinded_beacon_block: signed_blinded_block.clone(),
                request_id,
            })
            .await;

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

        let (head_slot, slot_duty) = self.curr_slot_info.slot_info();

        info!(%head_slot, request_ts = trace.receive, %block_hash);

        // Verify that the request is for the current slot
        if signed_blinded_block.message().slot() <= head_slot {
            warn!("request for past slot");
            return Err(ProposerApiError::RequestForPastSlot {
                request_slot: signed_blinded_block.message().slot(),
                head_slot,
            });
        }

        // Verify that we have a proposer connected for the current proposal
        let Some(slot_duty) = slot_duty else {
            warn!("no slot proposer duty");
            return Err(ProposerApiError::ProposerNotRegistered);
        };

        if let Err(err) = validate_proposal_coordinate(&signed_blinded_block, &slot_duty, head_slot)
        {
            warn!(%err, "invalid proposal coordinate");
            return Err(err);
        }
        trace.proposer_index_validated = utcnow_ns();

        let proposer_public_key = slot_duty.entry.registration.message.pubkey;
        if let Err(err) = verify_signed_blinded_block_signature(
            &self.chain_info,
            &mut signed_blinded_block,
            &proposer_public_key,
            self.chain_info.genesis_validators_root,
            &self.chain_info.context,
        ) {
            warn!(%err, "invalid signature");
            return Err(ProposerApiError::InvalidSignature);
        }
        trace.signature_validated = utcnow_ns();

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

        if let Err(err) = validate_block_equality(&versioned_payload, &signed_blinded_block) {
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

        let self_clone = self.clone();
        let unblinded_payload_clone = unblinded_payload.clone();
        let mut trace_clone = *trace;
        let payload_clone = payload.clone();

        let handle = task::spawn(file!(), line!(), async move {
            let mut failed_publishing = false;

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
                failed_publishing = true;
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

            (trace_clone, failed_publishing)
        });

        if !is_trusted_proposer {
            let Ok((new_trace, failed_publishing)) = handle.await else {
                return Err(ProposerApiError::InternalServerError);
            };
            *trace = new_trace;

            if failed_publishing {
                error!("failed to publish payload to beacon client");
                return Err(ProposerApiError::InternalServerError);
            }

            info!(?trace, "payload published and saved!");

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

    async fn is_trusted_proposer(
        &self,
        public_key: &BlsPublicKey,
    ) -> Result<bool, ProposerApiError> {
        let is_trusted_proposer = self.auctioneer.is_trusted_proposer(public_key).await?;
        Ok(is_trusted_proposer)
    }

    /// Fetches the execution payload associated with a given slot, public key, and block hash.
    ///
    /// The function will retry until the slot cutoff is reached.
    pub(crate) async fn get_execution_payload(
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

        if let Ok(Some((builder_pubkey, payload_address))) =
            self.auctioneer.get_payload_url(block_hash).await
        {
            // Fetch v3 optimistic payload from builder. This will complete asynchronously.
            info!(
                ?builder_pubkey,
                payload_address = String::from_utf8_lossy(&payload_address).as_ref(),
                "Requesting v3 payload from builder"
            );
            if let Err(e) = self
                .v3_payload_request
                .send((slot, *block_hash, builder_pubkey, payload_address))
                .await
            {
                error!("Failed to send v3 payload request: {e:?}");
            }
        }

        let mut last_error: Option<ProposerApiError> = None;
        let mut retry = 0; // Try at least once to cover case where get_payload is called too late.
        while retry == 0 || utcnow_ms() < slot_cutoff_millis {
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
                    if retry % 10 == 0 {
                        // 10 * RETRY_DELAY = 200ms
                        warn!("execution payload not found");
                    }
                    if retry == 0 && request_missing_payload {
                        let proposer_pubkey_clone = pub_key.clone();
                        let self_clone = self.clone();
                        let block_hash = *block_hash;

                        task::spawn(
                            file!(),
                            line!(),
                            async move {
                                self_clone
                                    .gossiper
                                    .request_payload(RequestPayloadParams {
                                        slot,
                                        proposer_pub_key: proposer_pubkey_clone,
                                        block_hash,
                                    })
                                    .await
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

            retry += 1;
            sleep(RETRY_DELAY).await;
        }

        error!("max retries reached trying to fetch execution payload");
        Err(last_error.unwrap_or_else(|| ProposerApiError::NoExecutionPayloadFound))
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

    /// `broadcast_signed_block` sends the provided signed block to all registered broadcasters
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
}

async fn deserialize_get_payload_bytes(
    req: Request<Body>,
) -> Result<SignedBlindedBeaconBlock, ProposerApiError> {
    let body = req.into_body();
    let body_bytes = to_bytes(body, MAX_BLINDED_BLOCK_LENGTH).await?;
    Ok(serde_json::from_slice(&body_bytes)?)
}

/// Validates the proposal coordinate of a given `SignedBlindedBeaconBlock`.
///
/// - Compares the proposer index of the block with the expected index for the current slot.
/// - Compares the api `head_slot` with the `slot_duty` slot.
/// - Compares the `slot_duty.slot` with the signed blinded block slot.
fn validate_proposal_coordinate(
    signed_blinded_block: &SignedBlindedBeaconBlock,
    slot_duty: &BuilderGetValidatorsResponseEntry,
    head_slot: Slot,
) -> Result<(), ProposerApiError> {
    let actual_index = signed_blinded_block.message().proposer_index();
    let expected_index = slot_duty.validator_index;

    if expected_index != actual_index {
        return Err(ProposerApiError::UnexpectedProposerIndex {
            expected: expected_index,
            actual: actual_index,
        });
    }

    if head_slot + 1 != slot_duty.slot {
        return Err(ProposerApiError::InternalSlotMismatchesWithSlotDuty {
            internal_slot: head_slot,
            slot_duty_slot: slot_duty.slot,
        });
    }

    if slot_duty.slot != signed_blinded_block.message().slot() {
        return Err(ProposerApiError::InvalidBlindedBlockSlot {
            internal_slot: slot_duty.slot,
            blinded_block_slot: signed_blinded_block.message().slot(),
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
    local_versioned_payload: &PayloadAndBlobs,
    provided_signed_blinded_block: &SignedBlindedBeaconBlock,
) -> Result<(), ProposerApiError> {
    let message = provided_signed_blinded_block.message();
    let body = message.body();

    let local_header = local_versioned_payload.execution_payload.to_ref().into();

    match local_header {
        ExecutionPayloadHeader::Bellatrix(_) |
        ExecutionPayloadHeader::Capella(_) |
        ExecutionPayloadHeader::Deneb(_) |
        ExecutionPayloadHeader::Fulu(_) => {
            return Err(ProposerApiError::UnsupportedBeaconChainVersion)
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

            if !local_kzg_commitments.iter().eq(provided_kzg_commitments.iter().map(|p| p.0)) {
                return Err(ProposerApiError::BlobKzgCommitmentsMismatch);
            }
        }
    }

    Ok(())
}

fn verify_signed_blinded_block_signature(
    chain_info: &ChainInfo,
    signed_blinded_beacon_block: &mut SignedBlindedBeaconBlock,
    public_key: &BlsPublicKey,
    genesis_validators_root: B256,
    context: &ChainSpec,
) -> Result<(), SigError> {
    let slot = signed_blinded_beacon_block.message().slot();
    let epoch = slot.epoch(chain_info.slots_per_epoch());
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
