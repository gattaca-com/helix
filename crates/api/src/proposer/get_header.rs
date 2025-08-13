use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use alloy_primitives::{Address, B256, U256};
use axum::{extract::Path, http::HeaderMap, response::IntoResponse, Extension};
use helix_common::{
    chain_info::ChainInfo,
    metadata_provider::MetadataProvider,
    metrics::GetHeaderMetric,
    resign_builder_bid, task,
    utils::{extract_request_id, utcnow_ms, utcnow_ns},
    BidRequest, GetHeaderTrace,
};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use helix_types::{
    BlsPublicKey, BuilderBid, BuilderBidElectra, ExecutionPayloadHeader, MergeableOrders,
    PayloadAndBlobs, PayloadAndBlobsRef,
};
use tokio::time::sleep;
use tracing::{debug, error, info, warn, Instrument};

use super::ProposerApi;
use crate::{
    builder::BlockMergeRequest,
    gossiper::types::RequestPayloadParams,
    proposer::{error::ProposerApiError, GetHeaderParams, GET_HEADER_REQUEST_CUTOFF_MS},
    Api, HEADER_TIMEOUT_MS,
};

impl<A: Api> ProposerApi<A> {
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
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        Extension(on_receive_ns): Extension<u64>,
        Extension(terminating): Extension<Arc<AtomicBool>>,
        headers: HeaderMap,
        Path(GetHeaderParams { slot, parent_hash, pubkey }): Path<GetHeaderParams>,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        if terminating.load(Ordering::Relaxed)
            || proposer_api.auctioneer.kill_switch_enabled().await?
        {
            return Err(ProposerApiError::ServiceUnavailableError);
        }

        let mut trace = GetHeaderTrace { receive: on_receive_ns, ..Default::default() };

        let (head_slot, duty) = proposer_api.curr_slot_info.slot_info();
        debug!(
            %head_slot,
            request_ts = trace.receive,
            %slot,
            %parent_hash,
            %pubkey,
        );

        let bid_request = BidRequest { slot: slot.into(), parent_hash, pubkey };

        // Dont allow requests for past slots
        if bid_request.slot < head_slot {
            debug!("request for past slot");
            return Err(ProposerApiError::RequestForPastSlot {
                request_slot: bid_request.slot,
                head_slot,
            });
        }

        // Only return a bid if there is a proposer connected this slot.
        let Some(duty) = duty else {
            debug!("proposer duty not found");
            return Err(ProposerApiError::ProposerNotRegistered);
        };

        let ms_into_slot = match validate_bid_request_time(&proposer_api.chain_info, &bid_request) {
            Ok(ms_into_slot) => ms_into_slot,
            Err(err) => {
                warn!(%err, "invalid bid request time");
                return Err(err);
            }
        };
        trace.validation_complete = utcnow_ns();

        let user_agent = proposer_api.metadata_provider.get_metadata(&headers);

        let mut mev_boost = false;

        // how far is the client
        let client_latency_ms = match get_x_mev_boost_header_start_ms(&headers) {
            Some(request_initiated_ms) => {
                let latency = utcnow_ms().saturating_sub(request_initiated_ms);
                mev_boost = true;
                latency * 105 / 100 // add some buffer
            }
            None => proposer_api.relay_config.timing_game_config.default_client_latency_ms,
        };

        info!(client_latency_ms, mev_boost, "request latency");

        let client_timeout_ms = headers
            .get(HEADER_TIMEOUT_MS)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| match h.parse::<u64>() {
                Ok(delay) => {
                    // TODO: move to debug at some point
                    info!("header timeout ms: {}", delay);
                    Some(delay)
                }
                Err(err) => {
                    warn!(%err, "invalid header timeout ms");
                    None
                }
            });

        let mut new_bid = None;

        // If timing games are enabled for the proposer then we sleep a fixed amount before
        // returning the header
        if duty.entry.preferences.header_delay || client_timeout_ms.is_some() {
            let max_sleep_time = proposer_api
                .relay_config
                .timing_game_config
                .latest_header_delay_ms_in_slot
                .saturating_sub(ms_into_slot);

            let target_sleep_time = match client_timeout_ms {
                Some(timeout_ms) => timeout_ms.saturating_sub(client_latency_ms),
                None => {
                    // TODO: convert this to a timeout instead of a delay
                    duty.entry
                        .preferences
                        .delay_ms
                        .unwrap_or(proposer_api.relay_config.timing_game_config.max_header_delay_ms)
                }
            };

            let sleep_time_ms = std::cmp::min(max_sleep_time, target_sleep_time);

            let sleep_time = Duration::from_millis(sleep_time_ms);
            let mut get_header_metric = GetHeaderMetric::new(sleep_time);

            debug!(target: "timing_games", 
                ?sleep_time,
                %ms_into_slot,
                target_sleep_time,
                max_sleep_time,
                slot,
                pubkey = ?bid_request.pubkey,
                "timing game sleep");

            // If time buffer is bigger than sleep time, wait for a bit before block merging
            let block_merging_buffer = Duration::from_millis(
                proposer_api.relay_config.block_merging_config.block_merging_buffer_ms,
            );
            let remaining_sleep_time = if sleep_time > block_merging_buffer {
                sleep(sleep_time - block_merging_buffer).await;
                block_merging_buffer
            } else {
                sleep_time
            };

            // Start block merging in the background
            let block_merging = tokio::spawn({
                let proposer_api = proposer_api.clone();
                let bid_request = bid_request.clone();
                async move {
                    proposer_api
                        .merge_block(&bid_request, duty.entry.registration.message.fee_recipient)
                        .await
                }
            });

            if remaining_sleep_time > Duration::ZERO {
                sleep(remaining_sleep_time).await;
            }

            get_header_metric.record();

            // If block merging didn't finish, we abort it
            if block_merging.is_finished() {
                new_bid = block_merging.await.ok().flatten();
            } else {
                block_merging.abort();
            }
        }

        let get_best_bid_res = proposer_api.shared_best_header.load(
            bid_request.slot.into(),
            &bid_request.parent_hash,
            &bid_request.pubkey,
        );

        trace.best_bid_fetched = utcnow_ns();
        debug!(trace = ?trace, "best bid fetched");

        let bid = match (get_best_bid_res, new_bid) {
            (Some((bid, _)), None) => bid,
            (None, Some(bid)) => bid,
            // If the merged payload has a higher value, we use that
            (Some((bid, _)), Some(new_bid)) if new_bid.value() > bid.value() => new_bid,
            // Otherwise, we use the top bid
            (Some((bid, _)), Some(_new_bid)) => bid,
            (None, None) => {
                warn!("no bid found");
                return Err(ProposerApiError::NoBidPrepared);
            }
        };
        if bid.value() == &U256::ZERO {
            warn!("best bid value is 0");
            return Err(ProposerApiError::BidValueZero);
        }

        debug!(
            value = ?bid.value(),
            block_hash = ?bid.header().block_hash(),
            "delivering bid",
        );

        // Save trace to DB
        save_get_header_call(
            proposer_api.db.clone(),
            slot,
            bid_request.parent_hash,
            bid_request.pubkey.clone(),
            bid.header().block_hash().0,
            trace,
            mev_boost,
            user_agent.clone(),
        )
        .await;

        let proposer_pubkey = bid_request.pubkey.clone();
        let block_hash = bid.header().block_hash().0;

        let fork = if bid.as_electra().is_ok() {
            helix_types::ForkName::Electra
        } else {
            error!("builder bid is not on Electra fork!! This should not happen");
            return Err(ProposerApiError::InternalServerError);
        };

        let signed_bid = resign_builder_bid(bid, &proposer_api.signing_context, fork);

        if user_agent.is_some() && is_mev_boost_client(&user_agent.unwrap()) {
            // Request payload in the background
            task::spawn(file!(), line!(), async move {
                proposer_api
                    .gossiper
                    .request_payload(RequestPayloadParams {
                        slot,
                        proposer_pub_key: proposer_pubkey,
                        block_hash,
                    })
                    .await
            });
        }

        let signed_bid = serde_json::to_value(signed_bid)?;
        info!(%signed_bid, "delivering bid");

        Ok(axum::Json(signed_bid))
    }

    async fn merge_block(
        &self,
        bid_request: &BidRequest,
        proposer_fee_recipient: Address,
    ) -> Option<BuilderBid> {
        debug!("merging block");

        let slot = bid_request.slot.into();

        let (bid, metadata) =
            self.shared_best_header.load(slot, &bid_request.parent_hash, &bid_request.pubkey)?;

        let block_hash = bid.header().block_hash().0;

        // If block does not allow merging, we stop
        if !metadata.allow_appending {
            return None;
        }

        let mergeable_orders = self.shared_best_orders.load();

        // Get execution payload from auctioneer
        let payload = self
            .get_execution_payload(bid_request.slot.into(), &bid_request.pubkey, &block_hash, true)
            .await
            .ok()?;

        let new_bid = self
            .append_transactions_to_payload(
                bid_request.slot.into(),
                proposer_fee_recipient,
                &bid_request.pubkey,
                bid,
                payload,
                mergeable_orders,
            )
            .await?;

        Some(new_bid)
    }

    /// Appends transactions to the payload and returns a new bid.
    /// The payload referenced in the bid is stored in the DB.
    /// This function might return the original bid if somehow the merged payload has a lower value.
    async fn append_transactions_to_payload(
        &self,
        slot: u64,
        proposer_fee_recipient: Address,
        proposer_pubkey: &BlsPublicKey,
        bid: BuilderBid,
        payload: PayloadAndBlobs,
        merging_data: Vec<MergeableOrders>,
    ) -> Option<BuilderBid> {
        let merge_request = BlockMergeRequest::new(
            *bid.value(),
            proposer_fee_recipient,
            payload.execution_payload,
            payload.blobs_bundle,
            merging_data,
        );

        let response = self.simulator.process_merge_request(merge_request).await.ok()?;

        // Sanity check: if the merged payload has a lower value than the original bid,
        // we return the original bid.
        if bid.value() >= &response.value {
            warn!(
                original_value = %bid.value(),
                %response.value,
                "merged payload has lower value than original bid"
            );
            return None;
        }
        let header = ExecutionPayloadHeader::from(response.execution_payload.to_ref())
            .as_electra()
            .ok()?
            .clone();
        let blob_kzg_commitments = response.blobs_bundle.commitments.clone();
        let block_hash = response.execution_payload.block_hash().0;

        let new_bid = BuilderBidElectra {
            header,
            blob_kzg_commitments,
            execution_requests: response.execution_requests,
            value: response.value,
            pubkey: *bid.pubkey(),
        };
        let payload_and_blobs = PayloadAndBlobsRef {
            execution_payload: (&response.execution_payload).into(),
            blobs_bundle: &response.blobs_bundle,
        };

        self.auctioneer
            .save_execution_payload(slot, proposer_pubkey, &block_hash, payload_and_blobs)
            .await
            .ok()?;

        Some(new_bid.into())
    }
}

async fn save_get_header_call<DB: DatabaseService + 'static>(
    db: Arc<DB>,
    slot: u64,
    parent_hash: B256,
    public_key: BlsPublicKey,
    best_block_hash: B256,
    trace: GetHeaderTrace,
    mev_boost: bool,
    user_agent: Option<String>,
) {
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

/// Validates that the bid request is not sent too late within the current slot.
///
/// - Only allows requests for the current slot until a certain cutoff time.
///
/// Returns how many ms we are into the slot if ok.
fn validate_bid_request_time(
    chain_info: &ChainInfo,
    bid_request: &BidRequest,
) -> Result<u64, ProposerApiError> {
    let curr_timestamp_ms = utcnow_ms() as i64;
    let slot_start_timestamp = chain_info.genesis_time_in_secs
        + (bid_request.slot.as_u64() * chain_info.seconds_per_slot());
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

pub fn is_mev_boost_client(client_name: &str) -> bool {
    let keywords = ["Kiln", "mev-boost", "commit-boost", "Vouch"];
    keywords.iter().any(|&keyword| client_name.contains(keyword))
}

/// Fetches the timestamp set by the mev-boost client when initialising the `get_header` request.
fn get_x_mev_boost_header_start_ms(header_map: &HeaderMap) -> Option<u64> {
    const MEV_BOOST_START_TIME_HEADER: &str = "X-MEVBoost-StartTimeUnixMS";
    const DATE_MS_HEADER: &str = "Date-Milliseconds";

    let header =
        header_map.get(DATE_MS_HEADER).or_else(|| header_map.get(MEV_BOOST_START_TIME_HEADER))?;
    let start_time_str = header.to_str().ok()?;
    let start_time_ms: u64 = start_time_str.parse().ok()?;
    Some(start_time_ms)
}
