use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use crossbeam_channel::{Receiver, Sender};
use dashmap::DashMap;
use flux::{spine::FluxSpine, tile::Tile};
use flux_network::{
    Token,
    tcp::{PollEvent, SendBehavior, TcpConnector, TcpTelemetry},
};
use helix_common::{SubmissionTrace, utils::utcnow_ns};
use helix_tcp_types::BidSubmission;
use helix_types::BlsPublicKeyBytes;
use ssz::{Decode, Encode};
use uuid::Uuid;

use crate::{
    AuctioneerHandle, HelixSpine, SubmissionResult,
    auctioneer::{InternalBidSubmissionHeader, SubmissionResultSender},
};

pub mod types;

pub use helix_tcp_types::{
    BidSubmissionFlags, BidSubmissionHeader, BidSubmissionResponse, RegistrationMsg,
};

pub use crate::tcp_bid_recv::types::{
    BidSubmissionError, response_from_bid_submission_error, response_from_builder_api_error,
};

const SUBMISSIONS_PER_SLOT: usize = 5000usize;

type SubmissionError = (Token, Option<u32>, Option<Uuid>, BidSubmissionError);

pub struct BidSubmissionTcpListener {
    listener: TcpConnector,

    auctioneer_handle: AuctioneerHandle,
    api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,

    rx: Receiver<SubmissionResult>,
    tx: Sender<SubmissionResult>,

    to_disconnect: Vec<Token>,
    registered: HashMap<Token, BlsPublicKeyBytes>,
    in_flight: HashMap<Uuid, (Token, u32)>,
    submission_errors: Vec<SubmissionError>,
}

impl BidSubmissionTcpListener {
    pub fn new(
        listener_addr: SocketAddr,
        auctioneer_handle: AuctioneerHandle,
        api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,
        max_connections: usize,
    ) -> Self {
        let mut listener = TcpConnector::default()
            .with_telemetry(TcpTelemetry::Enabled { app_name: HelixSpine::app_name() })
            .with_socket_buf_size(64 * 1024 * 1024); // 64MB

        let (tx, rx) = crossbeam_channel::bounded(10_000);

        listener.listen_at(listener_addr).expect("failed to initialise the TCP listener");

        Self {
            listener,
            auctioneer_handle,
            api_key_cache,
            rx,
            tx,
            to_disconnect: Vec::with_capacity(max_connections),
            registered: HashMap::with_capacity(max_connections),
            in_flight: HashMap::with_capacity(max_connections * SUBMISSIONS_PER_SLOT),
            submission_errors: Vec::with_capacity(max_connections),
        }
    }

    #[tracing::instrument(skip_all,
        fields(
        id =% request_id,
        slot = tracing::field::Empty,
        builder_pubkey = tracing::field::Empty,
        builder_id = tracing::field::Empty,
        block_hash = tracing::field::Empty,
    ))]
    fn accept_bid_submission(
        request_id: Uuid,
        tx: Sender<SubmissionResult>,
        auctioneer_handle: &AuctioneerHandle,
        bid: BidSubmission,
        expected_pubkey: BlsPublicKeyBytes,
    ) -> Result<(), BidSubmissionError> {
        let now = utcnow_ns();
        let trace = SubmissionTrace { receive: now, read_body: now, ..Default::default() };
        let header = InternalBidSubmissionHeader::from_tcp_header(request_id, bid.header);

        auctioneer_handle
            .block_submission(
                header,
                bid.data,
                trace,
                SubmissionResultSender::Shared(tx),
                Some(expected_pubkey),
            )
            .map_err(|_| BidSubmissionError::InternalError)
    }
}

impl Tile<HelixSpine> for BidSubmissionTcpListener {
    fn loop_body(&mut self, _adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        self.listener.poll_with(|event| match event {
            PollEvent::Accept { listener: _, stream, peer_addr } => {
                tracing::trace!("connected to new peer {:?} with token {:?}", peer_addr, stream);
            }
            PollEvent::Disconnect { token } => {
                self.registered.remove(&token);
            }
            PollEvent::Message { token, payload, .. } => {
                if let Some(expected_pubkey) = self.registered.get(&token) {
                    match BidSubmission::try_from(payload).map_err(BidSubmissionError::from) {
                        Ok(bid) => {
                            let request_id = Uuid::new_v4();
                            let seq_num = bid.header.sequence_number;
                            match Self::accept_bid_submission(
                                request_id,
                                self.tx.clone(),
                                &self.auctioneer_handle,
                                bid,
                                *expected_pubkey,
                            ) {
                                Ok(()) => {self.in_flight.insert(request_id, (token, seq_num));},
                                Err(e) => {
                                    tracing::error!(err=%e, id=%request_id, "failed to send bid submission to worker");
                                    self.submission_errors.push((
                                        token,
                                        Some(seq_num),
                                        Some(request_id),
                                        e,
                                    ));
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(err=%e, "failed to deserialize the bid");
                            self.submission_errors.push((token, None, None, e));
                        }
                    };
                } else {
                    let registration_message = RegistrationMsg::from_ssz_bytes(payload);
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

        for (request_id, result) in self.rx.try_iter() {
            if let Some((token, seq_num)) = self.in_flight.remove(&request_id) {
                let response = response_from_builder_api_error(seq_num, request_id, &result);
                self.listener.write_or_enqueue_with(SendBehavior::Single(token), |buffer| {
                    response.ssz_append(buffer);
                });
            } else {
                tracing::error!("no in-flight entry for request_id on submission result");
            }
        }
    }
}
