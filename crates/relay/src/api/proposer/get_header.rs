use std::{
    borrow::Cow,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{Extension, extract::Path, http::HeaderMap, response::IntoResponse};
use helix_common::{
    GET_HEADER_REQUEST_CUTOFF_MS, GetHeaderTrace, RequestTimings, ValidatorPreferences,
    api::proposer_api::GetHeaderParams,
    api_provider::{ApiProvider, TimingResult, header_ip_addr},
    chain_info::ChainInfo,
    decoder::{Encoding, HEADER_SSZ},
    metrics::{BID_SIGNING_LATENCY, HEADER_TIMEOUT_FETCH, HEADER_TIMEOUT_SLEEP},
    signing::RelaySigningContext,
    spawn_tracked,
    utils::{extract_request_id, utcnow_ms, utcnow_ns},
};
use helix_types::{BuilderBid, ForkName, GetHeaderResponse, SignedBuilderBid};
use http::{HeaderValue, header::CONTENT_TYPE};
use ssz::Encode;
use tracing::{Instrument, debug, error, info, trace, warn};

use super::ProposerApi;
use crate::api::{
    Api,
    proposer::{CONSENSUS_VERSION_HEADER, error::ProposerApiError},
    router::Terminating,
};

/// Gate for p2p payload sharing - get_header reqs in < 20% of previous epoch slots.  
const IP_FREQUENCY_THRESHOLD: f64 = 0.2;

pub(super) struct ValidatedHeaderRequest {
    pub ms_into_slot: u64,
    pub validation_complete_ns: u64,
    pub user_agent: Option<String>,
    pub is_mev_boost: bool,
    pub sleep_time: Option<Duration>,
    pub timeout_ms: Option<u64>,
    pub preferences: ValidatorPreferences,
    pub ip_addr: Option<IpAddr>,
}

impl<A: Api> ProposerApi<A> {
    pub(super) fn validate_header_request(
        &self,
        params: &GetHeaderParams,
        headers: &HeaderMap,
        terminating: &AtomicBool,
    ) -> Result<ValidatedHeaderRequest, ProposerApiError> {
        if terminating.load(Ordering::Relaxed) || self.local_cache.kill_switch_enabled() {
            return Err(ProposerApiError::ServiceUnavailableError);
        }

        let (head_slot, duty) = self.curr_slot_info.slot_info();
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

        let ms_into_slot = validate_bid_request_time(&self.chain_info, params)?;
        let validation_complete_ns = utcnow_ns();

        trace!(ms_into_slot, "completed validation");

        let user_agent = self.api_provider.get_metadata(headers);
        let ip_addr = header_ip_addr(headers);

        let TimingResult { is_mev_boost, sleep_time, timeout_ms } = self
            .api_provider
            .get_timing(params, headers, &duty.entry.preferences, ms_into_slot)
            .map_err(ProposerApiError::InvalidGetHeader)?;

        Ok(ValidatedHeaderRequest {
            ms_into_slot,
            validation_complete_ns,
            user_agent,
            is_mev_boost,
            sleep_time,
            timeout_ms,
            preferences: duty.entry.preferences,
            ip_addr,
        })
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
    #[tracing::instrument(skip_all, err(level = tracing::Level::TRACE), fields(id =% extract_request_id(&headers), slot = params.slot, parent_hash =? params.parent_hash))]
    pub async fn get_header(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        Extension(timings): Extension<RequestTimings>,
        Extension(Terminating(terminating)): Extension<Terminating>,
        headers: HeaderMap,
        Path(params): Path<GetHeaderParams>,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        trace!("starting call");

        let ValidatedHeaderRequest {
            ms_into_slot,
            validation_complete_ns,
            user_agent,
            is_mev_boost,
            sleep_time,
            ip_addr,
            ..
        } = proposer_api.validate_header_request(&params, &headers, &terminating)?;

        let mut trace = GetHeaderTrace {
            receive: timings.on_receive_ns,
            validation_complete: validation_complete_ns,
            ..Default::default()
        };

        let mut timing_guard = TimeoutGuard::<A> {
            reporter: ip_addr.map(|ip| (ip, proposer_api.api_provider.clone())),
            started: Instant::now(),
            ..Default::default()
        };

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

        let Ok(rx) = proposer_api.auctioneer_handle.get_header(params, is_mev_boost) else {
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
        let ep = bid.execution_payload();
        let builder_pubkey = *bid.bid_data_ref().builder_pubkey;
        let proposer_fee_recipient = ep.fee_recipient;
        let block_number = ep.block_number;
        let extra_data = ep.extra_data.to_vec();

        proposer_api.db.save_get_header_call(
            params,
            bid_block_hash,
            value,
            trace,
            is_mev_boost,
            user_agent,
            builder_pubkey,
            proposer_fee_recipient,
            block_number,
            extra_data,
        );

        let fork = proposer_api.chain_info.fork_at_slot(params.slot.into());
        let payload_and_blobs = bid.payload_and_blobs();
        let bid_data = bid.bid_data_ref().to_owned();
        let bid = bid.into_builder_bid_slow();
        let signed_bid = resign_builder_bid(bid, &proposer_api.signing_context, fork);

        if proposer_api.relay_config.gossip_payload_on_header && is_mev_boost {
            let ip_gate = ip_addr
                .map(|ip| proposer_api.ip_tracker.increment(params.slot, &ip))
                .unwrap_or_default();
            spawn_tracked!(
                async move {
                    info!("gossiping payload");
                    proposer_api
                        .gossip_payload(
                            params.slot.into(),
                            &params.pubkey,
                            Cow::Owned(payload_and_blobs),
                            fork,
                            Cow::Owned(bid_data),
                            ip_gate < IP_FREQUENCY_THRESHOLD,
                        )
                        .await;
                }
                .in_current_span()
            );
        }

        info!(block_hash =% bid_block_hash, ?value, "delivering bid");

        timing_guard.done_fetch = true;

        let response_encoding = Encoding::from_accept(&headers);

        match response_encoding {
            Encoding::Json => Ok(axum::Json(serde_json::to_value(signed_bid)?).into_response()),
            Encoding::Ssz => {
                let mut response = signed_bid.data.as_ssz_bytes().into_response();

                let headers = response.headers_mut();
                headers.insert(CONTENT_TYPE, HeaderValue::from_str(HEADER_SSZ).unwrap());
                headers.insert(
                    CONSENSUS_VERSION_HEADER,
                    HeaderValue::from_str(&fork.to_string()).unwrap(),
                );

                Ok(response)
            }
        }
    }
}

struct TimeoutGuard<A: Api> {
    done_sleep: bool,
    done_fetch: bool,
    started: Instant,
    reporter: Option<(IpAddr, Arc<A::ApiProvider>)>,
}

impl<A: Api> Default for TimeoutGuard<A> {
    fn default() -> Self {
        Self { done_sleep: false, done_fetch: false, started: Instant::now(), reporter: None }
    }
}

impl<A: Api> Drop for TimeoutGuard<A> {
    fn drop(&mut self) {
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        if !self.done_sleep {
            HEADER_TIMEOUT_SLEEP.inc();
            warn!("didn't complete sleep");
            if let Some((ip, provider)) = &self.reporter {
                provider.on_request_abandoned(*ip, elapsed_ms);
            }
        } else if !self.done_fetch {
            HEADER_TIMEOUT_FETCH.inc();
            warn!("didn't complete fetch")
        } else if let Some((ip, provider)) = &self.reporter {
            provider.on_request_completed(*ip, elapsed_ms);
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
