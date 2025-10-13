use std::{sync::Arc, time::Duration};

use axum::http::HeaderMap;
use tracing::{info, warn};

use crate::{
    RelayConfig, ValidatorPreferences, api::proposer_api::GetHeaderParams, utils::utcnow_ms,
};

pub const HEADER_TIMEOUT_MS: &str = "x-timeout-ms";

pub trait ApiProvider: Send + Sync + Clone + 'static {
    fn get_timing(
        &self,
        params: &GetHeaderParams,
        headers: &HeaderMap,
        preferences: &ValidatorPreferences,
        ms_into_slot: u64,
    ) -> Result<TimingResult, &'static str>;

    fn get_metadata(&self, headers: &HeaderMap) -> Option<String>;
}

pub struct TimingResult {
    pub sleep_time: Option<Duration>,
    pub is_mev_boost: bool,
}

#[derive(Clone)]
pub struct DefaultApiProvider {
    config: Arc<RelayConfig>,
}

impl DefaultApiProvider {
    pub fn new(config: Arc<RelayConfig>) -> Self {
        Self { config }
    }
}

impl ApiProvider for DefaultApiProvider {
    fn get_metadata(&self, headers: &HeaderMap) -> Option<String> {
        headers.get("user-agent").and_then(|v| v.to_str().ok()).map(|v| v.to_string())
    }

    fn get_timing(
        &self,
        _params: &GetHeaderParams,
        headers: &HeaderMap,
        preferences: &ValidatorPreferences,
        ms_into_slot: u64,
    ) -> Result<TimingResult, &'static str> {
        let mut is_mev_boost = false;

        // how far is the client
        let client_latency_ms = match get_x_mev_boost_header_start_ms(headers) {
            Some(request_initiated_ms) => {
                let latency = utcnow_ms().saturating_sub(request_initiated_ms);
                is_mev_boost = true;
                latency * 105 / 100 // add some buffer
            }
            None => self.config.timing_game_config.default_client_latency_ms,
        };

        info!(client_latency_ms, is_mev_boost, "request latency");

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
        if preferences.header_delay || client_timeout_ms.is_some() {
            let max_sleep_time = self
                .config
                .timing_game_config
                .latest_header_delay_ms_in_slot
                .saturating_sub(ms_into_slot);

            let target_sleep_time = match client_timeout_ms {
                Some(timeout_ms) => timeout_ms.saturating_sub(client_latency_ms),
                None => {
                    // TODO: convert this to a timeout instead of a delay
                    preferences
                        .delay_ms
                        .unwrap_or(self.config.timing_game_config.max_header_delay_ms)
                }
            };

            let sleep_time_ms = std::cmp::min(max_sleep_time, target_sleep_time);

            // leave additional time to resign the compute and sign the builder bid
            let sleep_time_adj = sleep_time_ms.saturating_sub(5);

            let sleep_time = Duration::from_millis(sleep_time_adj);

            Ok(TimingResult {
                sleep_time: (sleep_time > Duration::ZERO).then_some(sleep_time),
                is_mev_boost,
            })
        } else {
            Ok(TimingResult { sleep_time: None, is_mev_boost })
        }
    }
}

/// Fetches the timestamp set by the mev-boost client when initialising the `get_header` request.
pub fn get_x_mev_boost_header_start_ms(header_map: &HeaderMap) -> Option<u64> {
    const MEV_BOOST_START_TIME_HEADER: &str = "X-MEVBoost-StartTimeUnixMS";
    const DATE_MS_HEADER: &str = "Date-Milliseconds";

    let header =
        header_map.get(DATE_MS_HEADER).or_else(|| header_map.get(MEV_BOOST_START_TIME_HEADER))?;
    let start_time_str = header.to_str().ok()?;
    let start_time_ms: u64 = start_time_str.parse().ok()?;
    Some(start_time_ms)
}
