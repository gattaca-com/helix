use std::{
    sync::{Arc, atomic::Ordering},
    time::Instant,
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
use tracing::{Instrument, debug, error, info, trace, warn};

use super::ProposerApi;
use crate::api::{
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
    #[tracing::instrument(skip_all, err(level = tracing::Level::TRACE), fields(id =% extract_request_id(&headers), slot = params.slot, parent_hash =? params.parent_hash))]
    pub async fn get_header(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        Extension(timings): Extension<RequestTimings>,
        Extension(Terminating(terminating)): Extension<Terminating>,
        headers: HeaderMap,
        Path(params): Path<GetHeaderParams>,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        trace!("starting call");

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

        // Only return a bid if there is a proposer connected to this slot.
        let Some(duty) = duty else {
            debug!("proposer duty not found");
            return Err(ProposerApiError::ProposerNotRegistered);
        };

        let ms_into_slot = validate_bid_request_time(&proposer_api.chain_info, &params)?;
        trace.validation_complete = utcnow_ns();

        trace!(ms_into_slot, "completed validation");

        let user_agent = proposer_api.api_provider.get_metadata(&headers);

        let TimingResult { is_mev_boost, sleep_time } = proposer_api
            .api_provider
            .get_timing(&params, &headers, &duty.entry.preferences, ms_into_slot)
            .map_err(ProposerApiError::InvalidGetHeader)?;

        let mut timing_guard = TimeoutGuard::default();

        if let Some(sleep_time) = sleep_time {
            debug!(
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

        trace!("done sleep");

        let Ok(rx) = proposer_api.auctioneer_handle.get_header(params) else {
            error!("failed to send get_header to auctioneer");
            return Err(ProposerApiError::InternalServerError);
        };

        let bid = match rx.await {
            Ok(res) => res.inspect_err(|_| timing_guard.done_fetch = true)?,
            Err(err) => {
                warn!(%err, "failed to get header from auctioneer");
                return Err(ProposerApiError::InternalServerError);
            }
        };

        let now_ns = utcnow_ns();
        trace.best_bid_fetched = now_ns;
        debug!(trace = ?trace, "best bid fetched");

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
        let payload_and_blobs = bid.payload_and_blobs.clone();
        let bid_data = bid.bid_data.clone();
        let bid = bid.into_builder_bid_slow();
        let signed_bid = resign_builder_bid(bid, &proposer_api.signing_context, fork);

        if proposer_api.relay_config.gossip_payload_on_header && is_mev_boost {
            spawn_tracked!(
                async move {
                    info!("gossiping payload");
                    proposer_api
                        .gossip_payload(
                            params.slot.into(),
                            &params.pubkey,
                            &payload_and_blobs,
                            fork,
                            &bid_data,
                        )
                        .await;
                }
                .in_current_span()
            );
        }

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
        if !self.done_sleep {
            HEADER_TIMEOUT_SLEEP.inc();
            warn!("didn't complete sleep")
        } else if !self.done_fetch {
            HEADER_TIMEOUT_FETCH.inc();
            warn!("didn't complete fetch")
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
