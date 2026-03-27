use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use dashmap::DashMap;
use flux::{spine::FluxSpine, tile::Tile, timing::Nanos};
use flux_network::{
    Token,
    tcp::{PollEvent, SendBehavior, TcpConnector, TcpTelemetry},
};
use flux_utils::DCachePtr;
use helix_common::{SubmissionTrace, metrics::SUB_CLIENT_TO_SERVER_LATENCY, utils::utcnow_ns};
use helix_tcp_types::BID_SUB_HEADER_SIZE;
use helix_types::BlsPublicKeyBytes;
use ssz::{Decode, Encode};
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

pub struct BidSubmissionTcpListener {
    listener: TcpConnector,

    api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,

    to_disconnect: Vec<Token>,
    registered: HashMap<Token, BlsPublicKeyBytes>,
    submission_errors: Vec<SubmissionError>,
}

impl BidSubmissionTcpListener {
    pub fn new(
        listener_addr: SocketAddr,
        api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,
        max_connections: usize,
        dcache_ptr: DCachePtr,
    ) -> Self {
        let mut listener = TcpConnector::default()
            .with_telemetry(TcpTelemetry::Enabled { app_name: HelixSpine::app_name() })
            .with_socket_buf_size(64 * 1024 * 1024) // 64MB
            .with_dcache(dcache_ptr);
        listener.listen_at(listener_addr).expect("failed to initialise the TCP listener");

        Self {
            listener,
            api_key_cache,
            to_disconnect: Vec::with_capacity(max_connections),
            registered: HashMap::with_capacity(max_connections),
            submission_errors: Vec::with_capacity(max_connections),
        }
    }
}

impl Tile<HelixSpine> for BidSubmissionTcpListener {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        self.listener.poll_with_produce(&mut adapter.producers, |event| match event {
            PollEvent::Accept { listener: _, stream, peer_addr } => {
                tracing::trace!("connected to new peer {:?} with token {:?}", peer_addr, stream);
                None
            }
            PollEvent::Reconnect { token } => {
                tracing::trace!("reconnected to peer with token {:?}", token);
                None
            }
            PollEvent::Disconnect { token } => {
                tracing::trace!("disconnected from peer with token {:?}", token);
                self.registered.remove(&token);
                None
            }
            PollEvent::Message { token, payload, send_ts } => {
                if let Some(expected_pubkey) = self.registered.get(&token) {
                    let header = match BidSubmissionHeader::try_from(payload) {
                        Ok(header) => header,
                        Err(e) => {
                            tracing::error!("{e} failed to parse the bid submission header");
                            self.submission_errors.push((
                                token,
                                None,
                                None,
                                BidSubmissionError::ParseError(e),
                            ));
                            return None;
                        }
                    };

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
                        SubmissionRef::Tcp { id, token, seq_num: header.sequence_number };

                    let now = utcnow_ns();
                    SUB_CLIENT_TO_SERVER_LATENCY
                        .with_label_values(&["tcp"])
                        .observe((now.saturating_sub(send_ts.0) / 1000) as f64);

                    let trace = SubmissionTrace {
                        receive_ns: Nanos(now),
                        read_body_ns: Nanos(now),
                        ..Default::default()
                    };

                    Some(NewBidSubmission {
                        payload_offset: BID_SUB_HEADER_SIZE,
                        header: InternalBidSubmissionHeader::from_tcp_header(id, header),
                        submission_ref,
                        trace,
                        expected_pubkey: Some(*expected_pubkey),
                        http_submission_ix: None,
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
                            } else {
                                tracing::error!(
                                    "unknown api key and pubkey pair: {} {}, disconnecting peer",
                                    api_key,
                                    msg.builder_pubkey
                                );
                                self.to_disconnect.push(token);
                            }
                        }
                        Err(e) => {
                            tracing::error!(err=?e, "invalid registration message");
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
            self.listener.write_or_enqueue_with(SendBehavior::Single(token), |buffer| {
                response.ssz_append(buffer);
            });
        });
    }
}
