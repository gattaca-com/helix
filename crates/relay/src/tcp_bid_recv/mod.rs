use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use crossbeam_channel::{Receiver, Sender};
use dashmap::DashMap;
use flux::{spine::FluxSpine, tile::Tile};
use flux_network::{
    Token,
    tcp::{PollEvent, SendBehavior, TcpConnector, TcpTelemetry},
};
use helix_common::{SubmissionTrace, metrics::SUB_CLIENT_TO_SERVER_LATENCY, utils::utcnow_ns};
use helix_tcp_types::BidSubmission;
use helix_types::BlsPublicKeyBytes;
use ssz::{Decode, Encode};
use uuid::Uuid;

use crate::{
    AuctioneerHandle, HelixSpine, SubmissionResult,
    auctioneer::{InternalBidSubmissionHeader, SubmissionRef, SubmissionResultSender},
};

pub mod types;

pub use helix_tcp_types::{
    BidSubmissionFlags, BidSubmissionHeader, BidSubmissionResponse, RegistrationMsg,
};

pub use crate::tcp_bid_recv::types::{
    BidSubmissionError, response_from_bid_submission_error, response_from_builder_api_error,
};

type SubmissionError = (Token, Option<u32>, Option<Uuid>, BidSubmissionError);

pub struct BidSubmissionTcpListener {
    listener: TcpConnector,

    auctioneer_handle: AuctioneerHandle,
    api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,

    rx: Receiver<SubmissionResult>,
    tx: Sender<SubmissionResult>,

    to_disconnect: Vec<Token>,
    registered: HashMap<Token, BlsPublicKeyBytes>,
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
            submission_errors: Vec::with_capacity(max_connections),
        }
    }

    #[tracing::instrument(skip_all,
        fields(
        id =% submission_ref.id,
        slot = tracing::field::Empty,
        builder_pubkey = tracing::field::Empty,
        builder_id = tracing::field::Empty,
        block_hash = tracing::field::Empty,
    ))]
    fn accept_bid_submission(
        submission_ref: SubmissionRef,
        tx: Sender<SubmissionResult>,
        auctioneer_handle: &AuctioneerHandle,
        bid: BidSubmission,
        expected_pubkey: BlsPublicKeyBytes,
        trace: SubmissionTrace,
    ) -> Result<(), BidSubmissionError> {
        let header = InternalBidSubmissionHeader::from_tcp_header(submission_ref.id, bid.header);

        auctioneer_handle
            .block_submission(
                Some(submission_ref),
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
            },
            PollEvent::Reconnect { token } => {
                tracing::trace!("reconnected to peer with token {:?}", token);
            },
            PollEvent::Disconnect { token } => {
                tracing::trace!("disconnected from peer with token {:?}", token);
                self.registered.remove(&token);
            }
            PollEvent::Message { token, payload, send_ts } => {
                if let Some(expected_pubkey) = self.registered.get(&token) {
                    match BidSubmission::try_from(payload).map_err(BidSubmissionError::from) {
                        Ok(bid) => {
                            let submission_ref = SubmissionRef {
                                id: Uuid::new_v4(),
                                token,
                                seq_num: bid.header.sequence_number,
                            };

                            let now = utcnow_ns();
                            SUB_CLIENT_TO_SERVER_LATENCY.with_label_values(&["tcp"]).observe(
                                (now.saturating_sub(send_ts.0) / 1000) as f64
                            );

                            let trace = SubmissionTrace { receive_ns: now, read_body_ns: now, ..Default::default() };

                            if let Err(e) = Self::accept_bid_submission(
                                submission_ref,
                                self.tx.clone(),
                                &self.auctioneer_handle,
                                bid,
                                *expected_pubkey,
                                trace,
                            ) {
                                tracing::error!(err=%e, id=%submission_ref.id, "failed to send bid submission to worker");
                                self.submission_errors.push((
                                    submission_ref.token,
                                    Some(submission_ref.seq_num),
                                    Some(submission_ref.id),
                                    e,
                                ));
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

        for (submission_ref, result) in self.rx.try_iter() {
            if let Some(r) = submission_ref {
                let response = response_from_builder_api_error(r.seq_num, r.id, &result);
                tracing::debug!("submission result: {}", response);
                self.listener.write_or_enqueue_with(SendBehavior::Single(r.token), |buffer| {
                    response.ssz_append(buffer);
                });
            } else {
                tracing::error!("submission result has no SubmissionRef");
            }
        }
    }
}
