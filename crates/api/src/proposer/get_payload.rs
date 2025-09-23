use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use axum::{http::HeaderMap, response::IntoResponse, Extension};
use helix_beacon::types::BroadcastValidation;
use helix_common::{
    chain_info::ChainInfo,
    task,
    utils::{extract_request_id, utcnow_ms, utcnow_ns},
    GetPayloadTrace, RequestTimings,
};
use helix_database::DatabaseService;
use helix_types::{
    BlsPublicKeyBytes, GetPayloadResponse, PayloadAndBlobs, Slot, SlotClockTrait,
    VersionedSignedProposal,
};
use tokio::{sync::oneshot, time::sleep};
use tracing::{error, info, warn, Instrument};

use super::ProposerApi;
use crate::{
    builder::simulator_2::worker::GetPayloadResultData, constants::GET_PAYLOAD_REQUEST_CUTOFF_MS,
    gossiper::types::RequestPayloadParams, proposer::error::ProposerApiError, Api,
};

impl<A: Api> ProposerApi<A> {
    /// Implements this API: <https://ethereum.github.io/builder-specs/#/Builder/submitBlindedBlock>
    #[tracing::instrument(skip_all, fields(id), err)]
    pub async fn get_payload(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        Extension(timings): Extension<RequestTimings>,
        headers: HeaderMap,
        body: bytes::Bytes,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        let request_id = extract_request_id(&headers);
        tracing::Span::current().record("id", request_id.to_string());

        let trace = GetPayloadTrace::init_from_timings(timings);

        // TODO: l
        let slot = 0;
        let block_hash = Default::default();
        let proposer_pubkey = BlsPublicKeyBytes::default();

        // Handle early/late requests
        if let Err(err) = proposer_api.await_and_validate_slot_start_time(slot, trace.receive).await
        {
            warn!(error = %err, "get_payload was sent too late");

            // Save too late request to db for debugging
            if let Err(err) = proposer_api
                .db
                .save_too_late_get_payload(
                    slot,
                    &proposer_pubkey,
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

        // TODO: let user_agent = self.metadata_provider.get_metadata(&headers);
        let (tx, rx) = oneshot::channel();
        proposer_api
            .worker_tx
            .try_send(crate::builder::simulator_2::worker::WorkerJob::GetPayload {
                body,
                trace,
                res_tx: tx,
            })
            .expect("handle me");

        let res = rx.await.expect("handle me")?;

        match proposer_api._get_payload(res).await {
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
        GetPayloadResultData {
            to_proposer,
            to_publish,
            mut trace,
            proposer_pubkey,
            fork,
        }: GetPayloadResultData,
    ) -> Result<GetPayloadResponse, ProposerApiError> {
        let user_agent = None;
        let is_trusted_proposer = self.auctioneer.is_trusted_proposer(&proposer_pubkey);

        let self_clone = self.clone();
        let get_payload_response = to_proposer.clone();
        let handle = task::spawn(file!(), line!(), async move {
            let mut failed_publishing = false;

            if let Err(err) = self_clone
                .multi_beacon_client
                .publish_block(
                    Arc::new(to_publish), // TODO: dont arc here
                    Some(BroadcastValidation::ConsensusAndEquivocation),
                    fork,
                )
                .await
            {
                error!(%err, "error publishing block");
                failed_publishing = true;
            };

            trace.beacon_client_broadcast = utcnow_ns();

            // // Broadcast payload to all broadcasters
            // self_clone.broadcast_signed_block(
            //     unblinded_payload_clone.clone(),
            //     Some(BroadcastValidation::Gossip),
            // );
            trace.broadcaster_block_broadcast = utcnow_ns();

            // While we wait for the block to propagate, we also store the payload information
            trace.on_deliver_payload = utcnow_ns();
            self_clone
                .save_delivered_payload_info(to_proposer.data, proposer_pubkey, &trace, user_agent)
                .await;

            (trace, failed_publishing)
        });

        if !is_trusted_proposer {
            let Ok((trace, failed_publishing)) = handle.await else {
                return Err(ProposerApiError::InternalServerError);
            };

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

            // Return response
            info!(?trace, timestamp = utcnow_ns(), "delivering payload to untrusted proposer");
        } else {
            // Return response
            info!(timestamp = utcnow_ns(), "delivering payload to trusted proposer");
        }

        Ok(get_payload_response)
    }

    // TODO: tidy this up
    async fn await_and_validate_slot_start_time(
        &self,
        slot: u64,
        request_time_ns: u64,
    ) -> Result<(), ProposerApiError> {
        let Some((since_slot_start, until_slot_start)) =
            calculate_slot_time_info(&self.chain_info, slot.into(), request_time_ns)
        else {
            error!(request_time_ns, slot, "slot time info not found");
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

    /// Fetches the execution payload associated with a given slot, public key, and block hash.
    ///
    /// The function will retry until the slot cutoff is reached.
    pub(crate) async fn get_execution_payload(
        &self,
        slot: u64,
        pub_key: &BlsPublicKeyBytes,
        block_hash: &B256,
        request_missing_payload: bool,
    ) -> Result<PayloadAndBlobs, ProposerApiError> {
        const RETRY_DELAY: Duration = Duration::from_millis(20);

        let slot_time =
            self.chain_info.genesis_time_in_secs + (slot * self.chain_info.seconds_per_slot());
        let slot_cutoff_millis = (slot_time * 1000) + GET_PAYLOAD_REQUEST_CUTOFF_MS as u64;

        // let mut is_v3 = false;
        // let mut builder_pub_key = None;

        // if let Some((builder_pubkey, payload_address)) =
        // self.auctioneer.get_payload_url(block_hash) {
        //     // Fetch v3 optimistic payload from builder. This will complete asynchronously.
        //     info!(
        //         ?builder_pubkey,
        //         payload_address = String::from_utf8_lossy(&payload_address).as_ref(),
        //         "Requesting v3 payload from builder"
        //     );
        //     if let Err(e) = self
        //         .v3_payload_request
        //         .send((slot, *block_hash, builder_pubkey, payload_address))
        //         .await
        //     {
        //         error!("Failed to send v3 payload request: {e:?}");
        //     }

        //     is_v3 = true;
        //     builder_pub_key = Some(builder_pubkey);
        // }

        let mut retry = 0; // Try at least once to cover case where get_payload is called too late.
        while retry == 0 || utcnow_ms() < slot_cutoff_millis {
            match self.auctioneer.get_execution_payload(
                slot,
                pub_key,
                block_hash,
                self.chain_info.current_fork_name(),
            ) {
                Some(versioned_payload) => return Ok(versioned_payload),
                None => {
                    if retry % 10 == 0 {
                        // 10 * RETRY_DELAY = 200ms
                        warn!("execution payload not found");
                    }
                    if retry == 0 && request_missing_payload {
                        let proposer_pubkey_clone = *pub_key;
                        let gossiper = self.gossiper.clone();
                        let block_hash = *block_hash;

                        task::spawn(
                            file!(),
                            line!(),
                            async move {
                                gossiper
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
            }

            retry += 1;
            sleep(RETRY_DELAY).await;
        }

        // if is_v3 && self.auctioneer.has_header_been_served(block_hash) {
        //     warn!("v3 payload request was made, but no payload found. The builder may have failed
        // to deliver the payload in time.");     self.demote_builder(
        //         slot,
        //         &builder_pub_key.expect("builder pubkey must be set if is_v3 is true"),
        //         block_hash,
        //         "v3 header served but no payload found",
        //     )
        //     .await;
        // }

        error!("max retries reached trying to fetch execution payload");
        Err(ProposerApiError::NoExecutionPayloadFound)
    }

    async fn save_delivered_payload_info(
        &self,
        payload: Arc<PayloadAndBlobs>,
        proposer_public_key: BlsPublicKeyBytes,
        trace: &GetPayloadTrace,
        user_agent: Option<String>,
    ) {
        let db = self.db.clone();
        let trace = *trace;
        task::spawn(file!(), line!(), async move {
            if let Err(err) =
                db.save_delivered_payload(proposer_public_key, payload, &trace, user_agent).await
            {
                error!(%err, "error saving payload to database");
            }
        });
    }

    // async fn demote_builder(
    //     &self,
    //     slot: u64,
    //     builder: &BlsPublicKeyBytes,
    //     block_hash: &B256,
    //     reason: &str,
    // ) {
    //     if let Err(err) = self.auctioneer.demote_builder(builder) {
    //         error!(%err, %builder, "failed to demote builder in auctioneer");
    //     }

    //     if let Err(err) =
    //         self.db.db_demote_builder(slot, builder, block_hash, reason.to_string()).await
    //     {
    //         error!(%err,  %builder, "Failed to demote builder in database");
    //     }
    // }

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
            let broadcaster: Arc<helix_beacon::BlockBroadcaster> = broadcaster.clone();
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
