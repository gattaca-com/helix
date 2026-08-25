use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::{B256, U256};
use axum::{
    Extension,
    extract::{
        Path,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use helix_common::{
    GET_HEADER_REQUEST_CUTOFF_MS, GetHeaderTrace, RequestTimings,
    api::{HEADER_START_TIME_UNIX_MS, proposer_api::GetHeaderParams},
    api_provider::{ApiProvider, header_u64},
    utils::{extract_request_id, utcnow_ms, utcnow_ns},
};
use helix_types::{ForkName, SignedBuilderBid};
use http::HeaderMap;
use ssz::Encode;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{Instrument, debug, error, info, trace, warn};

use super::{
    ProposerApi,
    error::ProposerApiError,
    get_header::{ValidatedHeaderRequest, resign_builder_bid},
};
use crate::api::{Api, router::Terminating};

/// Frame layout: `[kind: u8][fork: u8][ssz SignedBuilderBid]`.
const FRAME_HEADER_LEN: usize = 2;
const FRAME_KIND_BID: u8 = 1;

impl<A: Api> ProposerApi<A> {
    #[tracing::instrument(skip_all, err(level = tracing::Level::TRACE), fields(id =% extract_request_id(&headers), slot = params.slot, parent_hash =? params.parent_hash))]
    pub async fn header_stream(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        Extension(timings): Extension<RequestTimings>,
        Extension(Terminating(terminating)): Extension<Terminating>,
        headers: HeaderMap,
        Path(params): Path<GetHeaderParams>,
        ws: WebSocketUpgrade,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        trace!("starting call");

        let config = &proposer_api.relay_config.header_stream;

        let ValidatedHeaderRequest {
            ms_into_slot,
            validation_complete_ns,
            user_agent,
            is_mev_boost,
            timeout_ms,
            preferences,
            ..
        } = proposer_api.validate_header_request(&params, &headers, &terminating)?;

        proposer_api
            .api_provider
            .admit_header_stream(&params, &headers, &preferences, config)
            .map_err(|reason| {
                warn!(slot = params.slot, proposer = %params.pubkey, reason, "refusing header stream");
                ProposerApiError::StreamNotAdmitted
            })?;

        let trace = GetHeaderTrace {
            receive: timings.on_receive_ns,
            validation_complete: validation_complete_ns,
            ..Default::default()
        };

        let Some(timeout_ms) = timeout_ms else {
            return Err(ProposerApiError::InvalidGetHeader("missing or invalid x-timeout-ms"));
        };

        let handshake_ms = header_u64(&headers, HEADER_START_TIME_UNIX_MS)
            .map(|start_time_ms| utcnow_ms().saturating_sub(start_time_ms))
            .unwrap_or(0);

        let window = stream_window(timeout_ms, handshake_ms, ms_into_slot);

        let now = Instant::now();
        let end = now + window;
        let start = now + window.saturating_sub(Duration::from_millis(config.stream_for_ms));
        let interval = Duration::from_millis(config.interval_ms);

        let fork = proposer_api.chain_info.fork_at_slot(params.slot.into());

        info!(
            ms_into_slot,
            is_mev_boost,
            timeout_ms,
            handshake_ms,
            ?fork,
            start_in_ms = (start - now).as_millis(),
            window_ms = (end - start).as_millis(),
            "accepting header stream"
        );

        let stream = HeaderStream {
            api: proposer_api,
            terminating,
            params,
            fork,
            is_mev_boost,
            user_agent,
            trace,
            start,
            end,
            interval,
        };

        Ok(ws
            .on_failed_upgrade(|error| warn!(%error, "header stream upgrade failed"))
            .on_upgrade(move |socket| stream.run(socket).in_current_span()))
    }
}

/// Per-connection state, moved into the task that owns the upgraded socket.
struct HeaderStream<A: Api> {
    api: Arc<ProposerApi<A>>,
    terminating: Arc<AtomicBool>,
    params: GetHeaderParams,
    fork: ForkName,
    is_mev_boost: bool,
    user_agent: Option<String>,
    trace: GetHeaderTrace,
    start: Instant,
    end: Instant,
    interval: Duration,
}

enum SendResult {
    Bid,
    Skipped,
    Closed,
}

impl<A: Api> HeaderStream<A> {
    async fn run(self, socket: WebSocket) {
        let mut delivered = 0;
        let hit_deadline =
            tokio::time::timeout_at(self.end, self.stream_bids(socket, &mut delivered))
                .await
                .is_err();

        info!(delivered, hit_deadline, "header stream closed");
    }

    async fn stream_bids(&self, mut socket: WebSocket, delivered: &mut usize) {
        let mut ticker = tokio::time::interval_at(self.start, self.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut last_sent = None;

        loop {
            ticker.tick().await;

            match self.send_bid(&mut socket, &mut last_sent).await {
                SendResult::Bid => *delivered += 1,
                SendResult::Skipped => {}
                SendResult::Closed => return,
            }
        }
    }

    async fn send_bid(
        &self,
        socket: &mut WebSocket,
        last_sent: &mut Option<(B256, U256)>,
    ) -> SendResult {
        if self.terminating.load(Ordering::Relaxed) {
            debug!("terminating, closing stream");
            return SendResult::Closed;
        }

        // TODO: time these
        let Ok(rx) = self.api.auctioneer_handle.get_header(self.params, self.is_mev_boost) else {
            error!("failed to send get_header to auctioneer");
            return SendResult::Skipped;
        };

        let bid = match rx.await {
            Ok(Ok(bid)) => bid,
            Ok(Err(err)) => {
                trace!(%err, "no bid to stream");
                return SendResult::Skipped;
            }
            Err(err) => {
                warn!(%err, "failed to get header from auctioneer");
                return SendResult::Skipped;
            }
        };

        let block_hash = *bid.block_hash();
        let value = *bid.value();
        if last_sent.replace((block_hash, value)) == Some((block_hash, value)) {
            return SendResult::Skipped;
        }

        let mut trace = self.trace;
        trace.best_bid_fetched = utcnow_ns();

        let ep = bid.execution_payload();
        let proposer_fee_recipient = ep.fee_recipient;
        let block_number = ep.block_number;
        let extra_data = ep.extra_data.to_vec();
        let builder_pubkey = *bid.bid_data_ref().builder_pubkey;

        // TODO: time these
        let signed_bid =
            resign_builder_bid(bid.into_builder_bid_slow(), &self.api.signing_context, self.fork);

        let frame = build_frame(self.fork, &signed_bid.data);
        if let Err(err) = socket.send(Message::binary(frame)).await {
            debug!(%err, "failed to send bid");
            return SendResult::Closed;
        }

        info!(%block_hash, ?value, "streamed bid");

        // TODO: we might want to save these separately with less data?
        self.api.db.save_get_header_call(
            self.params,
            block_hash,
            value,
            trace,
            self.is_mev_boost,
            self.user_agent.clone(),
            builder_pubkey,
            proposer_fee_recipient,
            block_number,
            extra_data,
        );

        SendResult::Bid
    }
}

fn stream_window(timeout_ms: u64, handshake_ms: u64, ms_into_slot: u64) -> Duration {
    Duration::from_millis(
        timeout_ms
            .saturating_sub(handshake_ms)
            .min((GET_HEADER_REQUEST_CUTOFF_MS as u64).saturating_sub(ms_into_slot)),
    )
}

fn build_frame(fork: ForkName, bid: &SignedBuilderBid) -> Vec<u8> {
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + bid.ssz_bytes_len());
    frame.push(FRAME_KIND_BID);
    frame.push(fork_byte(fork));
    bid.ssz_append(&mut frame);
    frame
}

fn fork_byte(fork: ForkName) -> u8 {
    match fork {
        ForkName::Base => 0,
        ForkName::Altair => 1,
        ForkName::Bellatrix => 2,
        ForkName::Capella => 3,
        ForkName::Deneb => 4,
        ForkName::Electra => 5,
        ForkName::Fulu => 6,
        ForkName::Gloas => 7,
        ForkName::Heze => 8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUTOFF: u64 = GET_HEADER_REQUEST_CUTOFF_MS as u64;

    #[test]
    fn test_stream_window() {
        // Instant handshake: the client's full budget
        assert_eq!(stream_window(950, 0, 0), Duration::from_millis(950));

        // The handshake is charged to the budget, so we stop when the client does
        assert_eq!(stream_window(950, 120, 0), Duration::from_millis(830));

        // Our own cutoff wins when the request comes late in the slot
        assert_eq!(stream_window(950, 0, CUTOFF - 200), Duration::from_millis(200));

        // Handshake longer than the budget, or a clock skewed that way: no window,
        // rather than an underflow
        assert_eq!(stream_window(950, 950, 0), Duration::ZERO);
        assert_eq!(stream_window(950, u64::MAX, 0), Duration::ZERO);

        // Past the cutoff entirely
        assert_eq!(stream_window(950, 0, CUTOFF + 1), Duration::ZERO);
    }
}
