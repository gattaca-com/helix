use std::{str::FromStr, sync::Arc, time::Duration};

use alloy_primitives::Address;
use axum::{Extension, http::HeaderMap, response::IntoResponse};
use helix_common::{
    Filtering, GetPayloadTrace, RequestTimings,
    api_provider::ApiProvider,
    chain_info::ChainInfo,
    spawn_tracked,
    utils::{extract_request_id, utcnow_ns},
};
use helix_types::{
    BlsPublicKeyBytes, ExecPayload, ForkName, GetPayloadResponse, PayloadAndBlobs,
    SignedBlindedBeaconBlock, SignedBlindedBeaconBlockElectra, SignedBlindedBeaconBlockFulu, Slot,
    SlotClockTrait,
};
use http::StatusCode;
use tokio::time::sleep;
use tracing::{Instrument, error, info, warn};

use super::ProposerApi;
use crate::{
    api::{Api, proposer::error::ProposerApiError},
    auctioneer::{GetPayloadResultData, PayloadBidData},
    beacon::types::BroadcastValidation,
    database::SavePayloadParams,
    gossip::{BroadcastGetPayloadParams, BroadcastPayloadParams},
};

const GET_PAYLOAD_REQUEST_CUTOFF_MS: i64 = 4000;
const CONSENSUS_VERSION_HEADER: &str = "Eth-Consensus-Version";

pub enum ProposerApiVersion {
    V1,
    V2,
}

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
    #[tracing::instrument(skip_all, fields(id, slot), err)]
    pub async fn get_payload(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        Extension(timings): Extension<RequestTimings>,
        headers: HeaderMap,
        body: bytes::Bytes,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        let request_id = extract_request_id(&headers);
        tracing::Span::current().record("id", request_id.to_string());

        let mut trace = GetPayloadTrace::init_from_timings(timings);

        let user_agent = proposer_api.api_provider.get_metadata(&headers);

        let fork: ForkName = match fork_name_from_header(&headers) {
            Ok(Some(fork)) => fork,
            err => {
                let curr = proposer_api.chain_info.current_fork_name();
                warn!(?err, "missing {CONSENSUS_VERSION_HEADER} header! using {curr:?}");
                curr
            }
        };

        // TODO: move decoding to worker
        let signed_blinded_block: SignedBlindedBeaconBlock = match fork {
            ForkName::Electra => {
                let signed_blinded_block_electra: SignedBlindedBeaconBlockElectra =
                    serde_json::from_slice(&body)
                        .inspect_err(|err| warn!(%err, "failed to deserialize signed block"))?;
                signed_blinded_block_electra.into()
            }
            ForkName::Fulu => {
                let signed_blinded_block_fulu: SignedBlindedBeaconBlockFulu =
                    serde_json::from_slice(&body)
                        .inspect_err(|err| warn!(%err, "failed to deserialize signed block"))?;
                signed_blinded_block_fulu.into()
            }
            _ => {
                return Err(ProposerApiError::InvalidFork);
            }
        };

        tracing::Span::current().record("slot", signed_blinded_block.slot().as_u64());

        trace.decode = utcnow_ns();

        let block_hash = signed_blinded_block
            .message()
            .body()
            .execution_payload()
            .map_err(|_| ProposerApiError::InvalidFork)? // this should never happen as post altair there's always an execution payload
            .block_hash()
            .0;

        let slot = signed_blinded_block.message().slot();

        let gossiper = proposer_api.gossiper.clone();
        let params = BroadcastGetPayloadParams {
            signed_blinded_beacon_block: signed_blinded_block.clone(),
            request_id,
        };

        spawn_tracked!(
            async move {
                info!("gossiping get payload");
                gossiper.broadcast_get_payload(params).await;
            }
            .in_current_span()
        );

        match proposer_api
            ._get_payload(signed_blinded_block, &mut trace, user_agent, ProposerApiVersion::V1)
            .await
        {
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
    /// Implements this API: <https://ethereum.github.io/builder-specs/#/Builder/submitBlindedBlockV2>
    #[tracing::instrument(skip_all, fields(id, slot), err)]
    pub async fn get_payload_v2(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        Extension(timings): Extension<RequestTimings>,
        headers: HeaderMap,
        body: bytes::Bytes,
    ) -> Result<StatusCode, ProposerApiError> {
        let request_id = extract_request_id(&headers);
        tracing::Span::current().record("id", request_id.to_string());

        let mut trace = GetPayloadTrace::init_from_timings(timings);

        let user_agent = proposer_api.api_provider.get_metadata(&headers);

        let fork: ForkName = match fork_name_from_header(&headers) {
            Ok(Some(fork)) => fork,
            err => {
                let curr = proposer_api.chain_info.current_fork_name();
                warn!(?err, "missing {CONSENSUS_VERSION_HEADER} header! using {curr:?}");
                curr
            }
        };

        // TODO: move decoding to worker
        let signed_blinded_block: SignedBlindedBeaconBlock = match fork {
            ForkName::Electra => {
                let signed_blinded_block_electra: SignedBlindedBeaconBlockElectra =
                    serde_json::from_slice(&body)
                        .inspect_err(|err| warn!(%err, "failed to deserialize signed block"))?;
                signed_blinded_block_electra.into()
            }
            ForkName::Fulu => {
                let signed_blinded_block_fulu: SignedBlindedBeaconBlockFulu =
                    serde_json::from_slice(&body)
                        .inspect_err(|err| warn!(%err, "failed to deserialize signed block"))?;
                signed_blinded_block_fulu.into()
            }
            _ => {
                return Err(ProposerApiError::InvalidFork);
            }
        };

        tracing::Span::current().record("slot", signed_blinded_block.slot().as_u64());

        trace.decode = utcnow_ns();

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

        match proposer_api
            ._get_payload(signed_blinded_block, &mut trace, user_agent, ProposerApiVersion::V2)
            .await
        {
            Ok(_get_payload_response) => Ok(StatusCode::ACCEPTED),
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
        signed_blinded_block: SignedBlindedBeaconBlock,
        trace: &mut GetPayloadTrace,
        user_agent: Option<String>,
        api_version: ProposerApiVersion,
    ) -> Result<GetPayloadResponse, ProposerApiError> {
        let block_hash = signed_blinded_block
            .message()
            .body()
            .execution_payload()
            .map_err(|_| ProposerApiError::InvalidFork)?
            .block_hash()
            .0;

        let (head_slot, slot_duty) = self.curr_slot_info.slot_info();
        // Verify that we have a proposer connected for the current proposal
        let Some(slot_duty) = slot_duty else {
            warn!("no slot proposer duty");
            return Err(ProposerApiError::ProposerNotRegistered);
        };

        let proposer_public_key = slot_duty.entry.registration.message.pubkey;
        let proposer_fee_recipient = slot_duty.entry.registration.message.fee_recipient;
        let filtering = slot_duty.entry.preferences.filtering;

        info!(%head_slot, request_ts = trace.receive, %block_hash);
        let slot = signed_blinded_block.message().slot();

        // Verify that the request is for the current slot
        if slot <= head_slot {
            warn!("request for past slot");
            return Err(ProposerApiError::RequestForPastSlot { request_slot: slot, head_slot });
        }

        let Ok(rx) =
            self.auctioneer_handle.get_payload(proposer_public_key, signed_blinded_block, *trace)
        else {
            error!("failed sending request to worker");
            return Err(ProposerApiError::InternalServerError);
        };

        let GetPayloadResultData { to_proposer, to_publish, trace: new_trace, fork, bid } = rx
            .await
            .inspect_err(|err| {
                error!(%err, "failed to receive payload response from auctioneer");
            })
            .map_err(|_| ProposerApiError::InternalServerError)??;

        *trace = new_trace;
        info!("found payload for blinded signed block");
        trace.payload_fetched = utcnow_ns();

        // Handle early/late requests
        if let Err(err) =
            self.await_and_validate_slot_start_time(head_slot + 1, trace.receive).await
        {
            warn!(error = %err, "get_payload was sent too late");

            // Save too late request to db for debugging
            if let Err(err) = self
                .db
                .save_too_late_get_payload(
                    (head_slot + 1).into(),
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

        self.gossip_payload(
            to_publish.signed_block.slot(),
            &proposer_public_key,
            &to_proposer.data,
            fork,
            &bid,
        )
        .await;

        let is_trusted_proposer = self.local_cache.is_trusted_proposer(&proposer_public_key);

        let self_clone = self.clone();
        let mut trace_clone = *trace;
        let payload_clone = to_proposer.data.clone();

        let handle = spawn_tracked!(async move {
            let mut failed_publishing = false;

            if let Err(err) = self_clone
                .multi_beacon_client
                .publish_block(
                    Arc::new(to_publish),
                    Some(BroadcastValidation::ConsensusAndEquivocation),
                    fork,
                )
                .await
            {
                error!(%err, "error publishing block");
                failed_publishing = true;
            };

            trace_clone.beacon_client_broadcast = utcnow_ns();

            trace_clone.broadcaster_block_broadcast = utcnow_ns();

            // While we wait for the block to propagate, we also store the payload information
            trace_clone.on_deliver_payload = utcnow_ns();
            self_clone
                .save_delivered_payload_info(
                    slot.as_u64(),
                    payload_clone,
                    &bid,
                    proposer_public_key,
                    proposer_fee_recipient,
                    &trace_clone,
                    user_agent,
                    filtering,
                )
                .await;

            (trace_clone, failed_publishing)
        });

        if let Some(merged_block) = self.local_cache.get_merged_block(&block_hash) {
            self.alert_manager.send(&merged_block.to_alert_message());
        }

        if !is_trusted_proposer && matches!(api_version, ProposerApiVersion::V1) {
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

        // Return response
        info!(?trace, timestamp = utcnow_ns(), "delivering payload");
        Ok(to_proposer)
    }

    async fn await_and_validate_slot_start_time(
        &self,
        slot: Slot,
        request_time_ns: u64,
    ) -> Result<(), ProposerApiError> {
        let Some((since_slot_start, until_slot_start)) =
            calculate_slot_time_info(&self.chain_info, slot, request_time_ns)
        else {
            error!(
                request_time_ns,
                %slot,
                "slot time info not found"
            );
            return Err(ProposerApiError::SlotTooNew);
        };

        if let Some(until_slot_start) = until_slot_start {
            info!("waiting until slot start t=0: {} ms", until_slot_start.as_millis());
            sleep(until_slot_start).await;
        } else if let Some(since_slot_start) = since_slot_start &&
            since_slot_start.as_millis() > GET_PAYLOAD_REQUEST_CUTOFF_MS as u128
        {
            return Err(ProposerApiError::GetPayloadRequestTooLate {
                cutoff: GET_PAYLOAD_REQUEST_CUTOFF_MS as u64,
                request_time: since_slot_start.as_millis() as u64,
            });
        }
        Ok(())
    }

    async fn save_delivered_payload_info(
        &self,
        slot: u64,
        payload: PayloadAndBlobs,
        bid: &PayloadBidData,
        proposer_public_key: BlsPublicKeyBytes,
        proposer_fee_recipient: Address,
        trace: &GetPayloadTrace,
        user_agent: Option<String>,
        filtering: Filtering,
    ) {
        let db = self.db.clone();
        let trace = *trace;
        let params = SavePayloadParams {
            slot,
            builder_pub_key: bid.builder_pubkey,
            proposer_pub_key: proposer_public_key,
            value: bid.value,
            proposer_fee_recipient,
            payload,
            latency_trace: trace,
            user_agent,
            filtering,
        };
        spawn_tracked!(async move {
            if let Err(err) = db.save_delivered_payload(&params).await {
                error!(%err, "error saving payload to database");
            }
        });
    }

    pub(crate) async fn gossip_payload(
        &self,
        bid_slot: Slot,
        proposer_public_key: &BlsPublicKeyBytes,
        execution_payload: &PayloadAndBlobs,
        fork_name: ForkName,
        bid: &PayloadBidData,
    ) {
        let params = BroadcastPayloadParams::to_proto(
            execution_payload,
            bid_slot.as_u64(),
            proposer_public_key,
            fork_name,
            bid,
        );
        self.gossiper.broadcast_payload(params).await
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

fn fork_name_from_header(headers: &HeaderMap) -> Result<Option<ForkName>, String> {
    headers
        .get(CONSENSUS_VERSION_HEADER)
        .map(|fork_name| fork_name.to_str().map_err(|e| e.to_string()).and_then(ForkName::from_str))
        .transpose()
}
