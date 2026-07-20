use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use dashmap::DashMap;
use flux::{tile::Tile, timing::Nanos};
use flux_network::{
    Token,
    tcp::{PollEvent, SendBehavior, TcpConnector, TcpTelemetry},
};
use flux_utils::{DCachePtr, SharedVector};
use helix_common::{SubmissionTrace, metrics::SUB_CLIENT_TO_SERVER_LATENCY, utils::utcnow_ns};
use helix_tcp_types::BID_SUB_HEADER_SIZE;
use helix_types::BlsPublicKeyBytes;
use ssz::{Decode, Encode};
use tracing::info;
use uuid::Uuid;

use crate::{
    HelixSpine,
    auctioneer::{InternalBidSubmissionHeader, SubmissionRef},
    spine::messages::{NewBidSubmission, SubmissionResultWithRef},
};

pub mod types;

pub use helix_tcp_types::{
    BidSubmissionFlags, BidSubmissionHeader, BidSubmissionResponse, RegistrationMsg,
};

pub use crate::tcp_bid_recv::types::{
    BidSubmissionError, response_from_bid_submission_error, response_from_submission_result,
};

type SubmissionError = (Token, Option<u32>, Option<Uuid>, BidSubmissionError);

/// Connections aren't slot-scoped, so this reports on a wall-clock cadence
/// instead of a slot boundary. For a registered peer, every `Message` is
/// either a decoded submission or a header-parse error:
/// `submissions_received + header_parse_errors == messages_from_registered`.
/// For an unregistered peer, every `Message` is a registration attempt:
/// `registration_ok + registration_invalid == registration_attempts`.
#[derive(Default)]
struct Stats {
    accepted: u32,
    reconnected: u32,
    disconnected: u32,
    registration_ok: u32,
    registration_invalid: u32,
    submissions_received: u32,
    header_parse_errors: u32,
    results_sent: u32,
}

pub struct BidSubmissionTcpListener {
    listener: TcpConnector,

    api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,

    to_disconnect: Vec<Token>,
    registered: HashMap<Token, BlsPublicKeyBytes>,
    submission_errors: Vec<SubmissionError>,

    // dcache bypass: stage a stable copy of the payload, the dcache slot can be
    // mutated out from under the decoder between publish and consume.
    http_submissions: Arc<SharedVector<Bytes>>,

    stats: Stats,
    next_report: Instant,
}

impl BidSubmissionTcpListener {
    const REPORT_FREQ: Duration = Duration::from_millis(500);

    pub fn new(
        listener_addr: SocketAddr,
        api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,
        max_connections: usize,
        dcache_ptr: DCachePtr,
        http_submissions: Arc<SharedVector<Bytes>>,
    ) -> Self {
        // TODO: enable telemetry once the per-connection shm queue leak is fixed
        // Telemetry creates 4 shm queues per accepted connection keyed by peer
        // addr incl. ephemeral port; never freed, so reconnect churn leaks
        // /dev/shm until the fix lands in flux.
        let mut listener = TcpConnector::default()
            .with_telemetry(TcpTelemetry::Disabled)
            .with_socket_buf_size(64 * 1024 * 1024) // 64MB
            .with_dcache(dcache_ptr);
        listener.listen_at(listener_addr).expect("failed to initialise the TCP listener");

        Self {
            listener,
            api_key_cache,
            to_disconnect: Vec::with_capacity(max_connections),
            registered: HashMap::with_capacity(max_connections),
            submission_errors: Vec::with_capacity(max_connections),
            http_submissions,
            stats: Stats::default(),
            next_report: Instant::now() + Self::REPORT_FREQ,
        }
    }

    fn maybe_report_stats(&mut self) {
        let now = Instant::now();
        if now < self.next_report {
            return;
        }
        self.next_report = now + Self::REPORT_FREQ;

        let stats = std::mem::take(&mut self.stats);
        info!(
            accepted = stats.accepted,
            reconnected = stats.reconnected,
            disconnected = stats.disconnected,
            registration_attempts = stats.registration_ok + stats.registration_invalid,
            registration_ok = stats.registration_ok,
            registration_invalid = stats.registration_invalid,
            messages_from_registered = stats.submissions_received + stats.header_parse_errors,
            submissions_received = stats.submissions_received,
            header_parse_errors = stats.header_parse_errors,
            results_sent = stats.results_sent,
            "tcp bid recv tile stats"
        );
    }
}

impl Tile<HelixSpine> for BidSubmissionTcpListener {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        self.listener.poll_with_produce(&mut adapter.producers, |event| match event {
            PollEvent::Accept { listener: _, stream, peer_addr } => {
                tracing::trace!("connected to new peer {:?} with token {:?}", peer_addr, stream);
                self.stats.accepted += 1;
                None
            }
            PollEvent::Reconnect { token } => {
                tracing::trace!("reconnected to peer with token {:?}", token);
                self.stats.reconnected += 1;
                None
            }
            PollEvent::Disconnect { token } => {
                tracing::trace!("disconnected from peer with token {:?}", token);
                self.registered.remove(&token);
                self.stats.disconnected += 1;
                None
            }
            PollEvent::Message { token, payload, send_ts } => {
                if let Some(expected_pubkey) = self.registered.get(&token) {
                    let header = match BidSubmissionHeader::try_from(payload) {
                        Ok(header) => header,
                        Err(e) => {
                            tracing::error!("{e} failed to parse the bid submission header");
                            self.stats.header_parse_errors += 1;
                            self.submission_errors.push((
                                token,
                                None,
                                None,
                                BidSubmissionError::Parse(e),
                            ));
                            return None;
                        }
                    };
                    self.stats.submissions_received += 1;

                    let id = Uuid::new_v4();
                    let _enter = tracing::info_span!(
                        "submit_block",
                        id = tracing::field::display(id),
                        slot = tracing::field::Empty,
                        builder_pubkey = tracing::field::Empty,
                        builder_id = tracing::field::Empty,
                        block_hash = tracing::field::Empty,
                    )
                    .entered();

                    let submission_ref =
                        SubmissionRef::Tcp { id, token: token.0, seq_num: header.sequence_number };

                    let now = utcnow_ns();
                    SUB_CLIENT_TO_SERVER_LATENCY
                        .with_label_values(&["tcp"])
                        .observe((now.saturating_sub(send_ts.0) / 1000) as f64);

                    let trace = SubmissionTrace {
                        receive_ns: Nanos(now),
                        read_body_ns: Nanos(now),
                        ..Default::default()
                    };

                    let http_submission_ix =
                        self.http_submissions.push(Bytes::copy_from_slice(payload));

                    Some(NewBidSubmission {
                        payload_offset: BID_SUB_HEADER_SIZE,
                        header: InternalBidSubmissionHeader::from_tcp_header(id, header),
                        submission_ref,
                        trace,
                        expected_pubkey: *expected_pubkey,
                        has_expected_pubkey: true,
                        http_submission_ix,
                    })
                } else {
                    match RegistrationMsg::from_ssz_bytes(payload) {
                        Ok(msg) => {
                            let api_key = Uuid::from_bytes(msg.api_key).to_string();
                            if self
                                .api_key_cache
                                .get(&api_key)
                                .is_some_and(|p| p.value().contains(&msg.builder_pubkey))
                            {
                                self.registered.insert(token, msg.builder_pubkey);
                                self.stats.registration_ok += 1;
                            } else {
                                tracing::error!(
                                    "unknown api key and pubkey pair: {} {}, disconnecting peer",
                                    api_key,
                                    msg.builder_pubkey
                                );
                                self.stats.registration_invalid += 1;
                                self.to_disconnect.push(token);
                            }
                        }
                        Err(e) => {
                            tracing::error!(err=?e, "invalid registration message");
                            self.stats.registration_invalid += 1;
                            self.to_disconnect.push(token);
                        }
                    }
                    None
                }
            }
        });

        for token in self.to_disconnect.drain(..) {
            self.listener.disconnect(token);
            self.registered.remove(&token);
        }

        for (token, seq_num, request_id, err) in self.submission_errors.drain(..) {
            self.listener.write_or_enqueue_with(SendBehavior::Single(token), |buffer| {
                response_from_bid_submission_error(seq_num, request_id, &err).ssz_append(buffer);
            });
        }

        adapter.consume(|r: SubmissionResultWithRef, _producers| {
            let SubmissionRef::Tcp { id, token, seq_num } = r.sub_ref else { return };
            let response =
                response_from_submission_result(seq_num, id, r.tcp_status, r.error_msg.as_str());
            tracing::debug!("submission result: {}", response);
            self.stats.results_sent += 1;
            self.listener.write_or_enqueue_with(SendBehavior::Single(Token(token)), |buffer| {
                response.ssz_append(buffer);
            });
        });

        self.maybe_report_stats();
    }
}
