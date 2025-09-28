use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use alloy_primitives::B256;
use axum::{extract::Path, http::HeaderMap, response::IntoResponse, Extension};
use helix_common::{
    chain_info::ChainInfo,
    metadata_provider::MetadataProvider,
    metrics::GetHeaderMetric,
    resign_builder_bid, task,
    utils::{extract_request_id, utcnow_ms, utcnow_ns},
    GetHeaderTrace, RequestTimings,
};
use helix_database::DatabaseService;
use helix_types::BlsPublicKeyBytes;
use tracing::{debug, error, info, warn, Instrument};

use super::ProposerApi;
use crate::{
    proposer::{error::ProposerApiError, GetHeaderParams, GET_HEADER_REQUEST_CUTOFF_MS},
    router::Terminating,
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
        Extension(timings): Extension<RequestTimings>,
        Extension(Terminating(terminating)): Extension<Terminating>,
        headers: HeaderMap,
        Path(params): Path<GetHeaderParams>,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        if terminating.load(Ordering::Relaxed) || proposer_api.local_cache.kill_switch_enabled() {
            return Err(ProposerApiError::ServiceUnavailableError);
        }

        let mut trace = GetHeaderTrace { receive: timings.on_receive_ns, ..Default::default() };

        let (head_slot, duty) = proposer_api.curr_slot_info.slot_info();
        let bid_slot = head_slot.as_u64() + 1;

        if params.slot != bid_slot {
            debug!("request for past slot");
            return Err(ProposerApiError::RequestWrongSlot { request_slot: params.slot, bid_slot });
        }

        // Only return a bid if there is a proposer connected this slot.
        let Some(duty) = duty else {
            debug!("proposer duty not found");
            return Err(ProposerApiError::ProposerNotRegistered);
        };

        let ms_into_slot = match validate_bid_request_time(&proposer_api.chain_info, &params) {
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
                slot = params.slot,
                pubkey = ?params.pubkey,
                "timing game sleep");

            if sleep_time > Duration::ZERO {
                tokio::time::sleep(sleep_time).await;
            }

            get_header_metric.record();
        }

        let Ok(rx) = proposer_api.auctioneer_handle.get_header(params) else {
            error!("failed to send get_header to auctioneer");
            return Err(ProposerApiError::ServiceUnavailableError)
        };

        // TODO: refactor errors
        let bid = rx.await.map_err(|_| ProposerApiError::NoBidPrepared)??;

        let now_ns = utcnow_ns();
        trace.best_bid_fetched = now_ns;
        debug!(trace = ?trace, "best bid fetched");

        // Try to fetch a merged block if block merging is enabled
        let bid = if proposer_api.relay_config.block_merging_config.is_enabled {
            let merged_block_bid =
                proposer_api.shared_best_merged.load(params.slot.into(), &params.parent_hash);
            let max_merged_bid_age_ms =
                proposer_api.relay_config.block_merging_config.max_merged_bid_age_ms;

            let now_ms = Duration::from_nanos(now_ns).as_millis() as u64;

            match merged_block_bid {
                None => bid,
                // If the current best bid has equal or higher value, we use that
                Some((_, merged_bid)) if merged_bid.value <= bid.value => bid,
                // If the merged bid is stale, we use the current best bid
                Some((time, _)) if time < now_ms - max_merged_bid_age_ms => bid,
                // Otherwise, we use the merged bid
                Some((_, merged_bid)) => merged_bid,
            }
        } else {
            bid
        };

        let bid_block_hash = bid.header.block_hash;
        debug!(
            value = ?bid.value,
            block_hash =% bid_block_hash,
            "delivering bid",
        );

        // Save trace to DB
        save_get_header_call(
            proposer_api.db.clone(),
            params.slot,
            params.parent_hash,
            params.pubkey,
            bid_block_hash,
            trace,
            mev_boost,
            user_agent.clone(),
        )
        .await;

        let fork = proposer_api.chain_info.current_fork_name();

        let signed_bid = resign_builder_bid(bid, &proposer_api.signing_context, fork);

        let signed_bid = serde_json::to_value(signed_bid)?;
        info!(block_hash =% bid_block_hash, "delivering bid");

        Ok(axum::Json(signed_bid))
    }
}

async fn save_get_header_call<DB: DatabaseService + 'static>(
    db: Arc<DB>,
    slot: u64,
    parent_hash: B256,
    public_key: BlsPublicKeyBytes,
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
    bid_request: &GetHeaderParams,
) -> Result<u64, ProposerApiError> {
    let curr_timestamp_ms = utcnow_ms() as i64;
    let slot_start_timestamp =
        chain_info.genesis_time_in_secs + (bid_request.slot * chain_info.seconds_per_slot());
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

// pub fn is_mev_boost_client(client_name: &str) -> bool {
//     let keywords = ["Kiln", "mev-boost", "commit-boost", "Vouch"];
//     keywords.iter().any(|&keyword| client_name.contains(keyword))
// }

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
