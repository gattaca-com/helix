use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use axum::{Extension, extract::Path, http::HeaderMap, response::IntoResponse};
use helix_common::{
    GetHeaderTrace, RequestTimings,
    api::proposer_api::GetHeaderParams,
    api_provider::{ApiProvider, TimingResult},
    chain_info::ChainInfo,
    metrics::{BID_SIGNING_LATENCY, HEADER_TIMEOUT_FETCH, HEADER_TIMEOUT_SLEEP},
    signing::RelaySigningContext,
    spawn_tracked,
    utils::{extract_request_id, utcnow_ms, utcnow_ns},
};
use helix_types::{BuilderBid, ForkName, GetHeaderResponse, SignedBuilderBid};
use tracing::{Instrument, debug, error, info, warn};

use super::ProposerApi;
use crate::{
    Api,
    proposer::{GET_HEADER_REQUEST_CUTOFF_MS, error::ProposerApiError},
    router::Terminating,
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
    #[tracing::instrument(skip_all, fields(id =% extract_request_id(&headers), slot = params.slot))]
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

        let ms_into_slot = validate_bid_request_time(&proposer_api.chain_info, &params)?;
        trace.validation_complete = utcnow_ns();

        let user_agent = proposer_api.api_provider.get_metadata(&headers);

        let TimingResult { is_mev_boost, sleep_time } = proposer_api
            .api_provider
            .get_timing(&params, &headers, &duty.entry.preferences, ms_into_slot)
            .map_err(ProposerApiError::InvalidGetHeader)?;

        let mut timing_guard = TimeoutGuard::default();

        if let Some(sleep_time) = sleep_time {
            debug!(target: "timing_games",
            ?sleep_time,
            ms_into_slot,
            slot = params.slot,
            pubkey = ?params.pubkey,
            "timing game sleep");

            tokio::time::sleep(sleep_time).await;
            timing_guard.done_sleep = true;
        } else {
            timing_guard.done_sleep = true;
        };

        let Ok(rx) = proposer_api.auctioneer_handle.get_header(params) else {
            error!("failed to send get_header to auctioneer");
            return Err(ProposerApiError::InternalServerError);
        };

        let local_bid = match rx.await {
            Ok(res) => res?,
            Err(err) => {
                warn!(%err, "failed to get header from auctioneer");
                return Err(ProposerApiError::InternalServerError);
            }
        };

        let now_ns = utcnow_ns();
        trace.best_bid_fetched = now_ns;
        debug!(trace = ?trace, "best bid fetched");

        // Try to fetch a merged block if block merging is enabled
        let bid = if proposer_api.relay_config.block_merging_config.is_enabled {
            let merged_bid = proposer_api.shared_best_merged.load(params.slot, &params.parent_hash);
            let max_merged_bid_age_ms =
                proposer_api.relay_config.block_merging_config.max_merged_bid_age_ms;

            let now_ms = Duration::from_nanos(now_ns).as_millis() as u64;

            match merged_bid {
                None => {
                    debug!(
                        "no merged bid found, using regular bid, value = {:?}, block_hash = {:?}",
                        local_bid.value(),
                        local_bid.block_hash()
                    );
                    local_bid
                }
                // If the current best bid has equal or higher value, we use that
                Some((_, merged_bid)) if merged_bid.value() <= local_bid.value() => {
                    debug!(
                        "merged bid {:?} with value {:?} is not higher than regular bid, using regular bid, value = {:?}, block_hash = {:?}",
                        merged_bid.value(),
                        merged_bid.block_hash(),
                        local_bid.value(),
                        local_bid.block_hash()
                    );
                    local_bid
                }
                // If the merged bid is stale, we use the current best bid
                Some((time, merged_bid)) if time < now_ms - max_merged_bid_age_ms => {
                    debug!(
                        "merged bid {:?} with value {:?} is stale ({} ms old), using regular bid, value = {:?}, block_hash = {:?}",
                        merged_bid.value(),
                        merged_bid.block_hash(),
                        now_ms - time,
                        local_bid.value(),
                        local_bid.block_hash()
                    );
                    local_bid
                }
                // Otherwise, we use the merged bid
                Some((_, merged_bid)) => {
                    debug!(
                        "using merged bid, value = {:?}, block_hash = {:?}",
                        merged_bid.value(),
                        merged_bid.block_hash()
                    );
                    merged_bid
                }
            }
        } else {
            local_bid
        };

        let bid_block_hash = *bid.block_hash();
        let value = *bid.value();

        let db = proposer_api.db.clone();
        spawn_tracked!(
            async move {
                if let Err(err) = db
                    .save_get_header_call(params, bid_block_hash, trace, is_mev_boost, user_agent)
                    .await
                {
                    error!(%err, "error saving get header call to database");
                }
            }
            .in_current_span()
        );

        let fork = proposer_api.chain_info.current_fork_name();

        let bid = bid.to_builder_bid_slow();
        let signed_bid = resign_builder_bid(bid, &proposer_api.signing_context, fork);

        let signed_bid = serde_json::to_value(signed_bid)?;
        info!(block_hash =% bid_block_hash, ?value, "delivering bid");

        timing_guard.done_fetch = true;
        Ok(axum::Json(signed_bid))
    }
}

#[derive(Default)]
struct TimeoutGuard {
    done_sleep: bool,
    done_fetch: bool,
}

impl Drop for TimeoutGuard {
    fn drop(&mut self) {
        if !self.done_fetch {
            HEADER_TIMEOUT_FETCH.inc();
            warn!("didn't complete fetch")
        } else if !self.done_sleep {
            HEADER_TIMEOUT_SLEEP.inc();
            warn!("didn't complete sleep")
        }
    }
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

/// Signs the builder bid with the relay key. This is necessary because the relay is the "builder"
/// from the proposer point of view
pub fn resign_builder_bid(
    mut message: BuilderBid,
    signing_ctx: &RelaySigningContext,
    fork: ForkName,
) -> GetHeaderResponse {
    let start = Instant::now();

    message.pubkey = *signing_ctx.pubkey();
    let sig = signing_ctx.sign_builder_message(&message).serialize().into();

    let bid = GetHeaderResponse {
        version: fork,
        metadata: Default::default(),
        data: SignedBuilderBid { message, signature: sig },
    };

    BID_SIGNING_LATENCY.observe(start.elapsed().as_micros() as f64);
    debug!("signing builder bid took {:?}", start.elapsed());

    bid
}
