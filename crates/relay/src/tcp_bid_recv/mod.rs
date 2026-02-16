use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use crossbeam_channel::{Receiver, Sender};
use dashmap::DashMap;
use flux::{spine::FluxSpine, tile::Tile};
use flux_network::{
    Token,
    tcp::{SendBehavior, TcpConnector, TcpTelemetry},
};
use helix_common::SubmissionTrace;
use helix_types::{BlsPublicKeyBytes, SeqNum};
use ssz::{Decode, Encode};
use uuid::Uuid;

use crate::{
    AuctioneerHandle, HelixSpine, SubmissionResult,
    auctioneer::{BlockSubResultSender, Encoding},
    tcp_bid_recv::types::{BidSubmission, BidSubmissionResponse, RegistrationMsg},
};

pub mod types;

pub use crate::tcp_bid_recv::types::{BidSubmissionError, BidSubmissionHeader};

pub struct BidSubmissionTcpListener {
    listener_addr: SocketAddr,
    listener: TcpConnector,
    listener_token: Option<Token>,

    auctioneer_handle: AuctioneerHandle,
    api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,

    rx: Receiver<SubmissionResult>,
    tx: Sender<SubmissionResult>,

    in_flight: HashMap<SeqNum, Token>,
    to_disconnect: Vec<Token>,
    registered: HashMap<Token, BlsPublicKeyBytes>,
    submission_errors: Vec<(Token, Option<SeqNum>, BidSubmissionError)>,
}

impl BidSubmissionTcpListener {
    pub fn new(
        listener_addr: SocketAddr,
        auctioneer_handle: AuctioneerHandle,
        api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,
        max_connections: usize,
    ) -> Self {
        let listener = TcpConnector::default()
            .with_telemetry(TcpTelemetry::Enabled { app_name: HelixSpine::app_name() })
            .with_socket_buf_size(64 * 1024 * 1024); // 64MB

        let (tx, rx) = crossbeam_channel::bounded(10_000);

        Self {
            listener_addr,
            listener,
            listener_token: None,
            auctioneer_handle,
            api_key_cache,
            rx,
            tx,
            in_flight: HashMap::with_capacity(max_connections),
            to_disconnect: Vec::with_capacity(max_connections),
            registered: HashMap::with_capacity(max_connections),
            submission_errors: Vec::with_capacity(max_connections),
        }
    }

    #[tracing::instrument(skip_all,
        fields(
        id =% Uuid::from_u128(bid.header.sequence_number),
        slot = tracing::field::Empty,
        builder_pubkey = tracing::field::Empty,
        builder_id = tracing::field::Empty,
        block_hash = tracing::field::Empty,
    ))]
    fn accept_bid_submission(
        tx: Sender<SubmissionResult>,
        auctioneer_handle: &AuctioneerHandle,
        bid: BidSubmission,
        expected_pubkey: BlsPublicKeyBytes,
    ) -> Result<(), BidSubmissionError> {
        let compression = bid.header.compression();

        auctioneer_handle
            .block_submission(
                bid.header,
                Some(bid.header.sequence_number),
                Encoding::Ssz,
                compression,
                None,
                bid.data,
                SubmissionTrace::default(),
                BlockSubResultSender::Shared(tx),
                Some(expected_pubkey),
            )
            .map_err(|_| BidSubmissionError::InternalError)
    }
}

impl Tile<HelixSpine> for BidSubmissionTcpListener {
    fn try_init(&mut self, _adapter: &mut flux::spine::SpineAdapter<HelixSpine>) -> bool {
        self.listener_token = self.listener.listen_at(self.listener_addr);
        self.listener_token.is_some()
    }

    fn loop_body(&mut self, _adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        self.listener.poll_with(
            |connection| {
                tracing::trace!(
                    "connected to new peer {:?} with token {:?}",
                    connection.peer_addr,
                    connection.incoming_stream
                );
            },
            |token, msg, _| {
                if let Some(expected_pubkey) = self.registered.get(&token) {
                    match BidSubmission::try_from(msg) {
                        Ok(bid) => {
                            let request_id = bid.header.sequence_number;
                            match Self::accept_bid_submission(self.tx.clone(), &self.auctioneer_handle, bid, *expected_pubkey) {
                                Ok(_) => {
                                    self.in_flight.insert(request_id, token);
                                }
                                Err(e) => {
                                    tracing::error!(err=%e, "failed to send bid submission to worker");
                                    self.submission_errors.push((token, Some(request_id), e));
                                }
                            }
                        },
                        Err(e) => {
                            tracing::error!(err=%e, "failed to deserialize the bid");
                            self.submission_errors.push((token, None, e));
                        }
                    };
                } else {
                    let registration_message =
                        RegistrationMsg::from_ssz_bytes(msg);
                    match registration_message {
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
                }
            },
        );

        for token in self.to_disconnect.drain(..) {
            self.listener.disconnect(token);
            self.registered.remove(&token);
        }

        for (token, request_id, err) in self.submission_errors.drain(..) {
            self.listener.write_or_enqueue_with(SendBehavior::Single(token), |buffer| {
                BidSubmissionResponse::from_bid_submission_error(&request_id, &err)
                    .ssz_append(buffer);
            });
        }

        for (seq_num, result) in self.rx.try_iter() {
            if let Some(token) = self.in_flight.remove(&seq_num) {
                self.listener.write_or_enqueue_with(SendBehavior::Single(token), |buffer| {
                    BidSubmissionResponse::from_builder_api_error(seq_num, &result)
                        .ssz_append(buffer);
                });
            } else {
                tracing::error!(id=%seq_num, "connection token not found for submission result: {:?}", result);
            }
        }
    }
}
