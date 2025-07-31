use std::{sync::Arc, time::Duration};

use alloy_primitives::{B256, U256};
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
use helix_types::BlsPublicKey;
use tokio::time::sleep;
use tracing::{debug, error, info, warn, Instrument};

use super::ProposerApi;
use crate::{
    gossiper::types::RequestPayloadParams,
    proposer::{error::ProposerApiError, GetHeaderParams, GET_HEADER_REQUEST_CUTOFF_MS},
    Api,
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
        headers: HeaderMap,
        Path(GetHeaderParams { slot, parent_hash, pubkey }): Path<GetHeaderParams>,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        if proposer_api.auctioneer.kill_switch_enabled().await? {
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
                pubkey = ?bid_request.pubkey,
                "timing game sleep");

            if sleep_time > Duration::ZERO {
                sleep(sleep_time).await;
            }

            get_header_metric.record();
        }

        let get_best_bid_res = proposer_api.shared_best_header.load(
            bid_request.slot.into(),
            &bid_request.parent_hash,
            &bid_request.pubkey,
        );

        trace.best_bid_fetched = utcnow_ns();
        debug!(trace = ?trace, "best bid fetched");

        if let Some(bid) = get_best_bid_res {
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

            let proposer_pubkey_clone = bid_request.pubkey;
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
                            proposer_pub_key: proposer_pubkey_clone,
                            block_hash,
                        })
                        .await
                });
            }

            let signed_bid = serde_json::to_value(signed_bid)?;
            info!(%signed_bid, "delivering bid");

            Ok(axum::Json(signed_bid))
        } else {
            warn!("no bid found");
            Err(ProposerApiError::NoBidPrepared)
        }
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
    let slot_start_timestamp = chain_info.genesis_time_in_secs +
        (bid_request.slot.as_u64() * chain_info.seconds_per_slot());
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
