use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Instant};

use bytes::Bytes;
use dashmap::DashMap;
use flux::{spine::FluxSpine, tile::Tile, timing::Nanos, utils::SharedVector};
use flux_network::{
    Token,
    tcp::{PollEvent, SendBehavior, TcpConnector, TcpTelemetry},
};
use helix_common::{SubmissionTrace, metrics::SUB_CLIENT_TO_SERVER_LATENCY, utils::utcnow_ns};
use helix_tcp_types::BidSubmission;
use helix_types::BlsPublicKeyBytes;
use ssz::{Decode, Encode};
use tokio::sync::mpsc;
use tracing::Span;
use uuid::Uuid;

use crate::{
    HelixSpine,
    auctioneer::{
        InternalBidSubmission, InternalBidSubmissionHeader, SubmissionRef, SubmissionResultWithRef,
    },
    spine::messages::{NewBidSubmissionIx, SubmissionResultIx},
};

mod s3;
pub mod types;

pub use helix_tcp_types::{
    BidSubmissionFlags, BidSubmissionHeader, BidSubmissionResponse, RegistrationMsg,
};
pub use s3::S3PayloadSaver;

pub use crate::tcp_bid_recv::types::{
    BidSubmissionError, response_from_bid_submission_error, response_from_builder_api_error,
};

type SubmissionError = (Token, Option<u32>, Option<Uuid>, BidSubmissionError);

pub struct BidSubmissionTcpListener {
    listener: TcpConnector,

    api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,

    to_disconnect: Vec<Token>,
    registered: HashMap<Token, BlsPublicKeyBytes>,
    submission_errors: Vec<SubmissionError>,
    raw_payloads_tx: Option<mpsc::Sender<(Uuid, Bytes)>>,
    submissions: Arc<SharedVector<InternalBidSubmission>>,
    submission_results: Arc<SharedVector<SubmissionResultWithRef>>,
}

impl BidSubmissionTcpListener {
    pub fn new(
        listener_addr: SocketAddr,
        api_key_cache: Arc<DashMap<String, Vec<BlsPublicKeyBytes>>>,
        max_connections: usize,
        raw_payloads_tx: Option<mpsc::Sender<(Uuid, Bytes)>>,
        submissions: Arc<SharedVector<InternalBidSubmission>>,
        submission_results: Arc<SharedVector<SubmissionResultWithRef>>,
    ) -> Self {
        let mut listener = TcpConnector::default()
            .with_telemetry(TcpTelemetry::Enabled { app_name: HelixSpine::app_name() })
            .with_socket_buf_size(64 * 1024 * 1024); // 64MB

        listener.listen_at(listener_addr).expect("failed to initialise the TCP listener");

        Self {
            listener,
            api_key_cache,
            to_disconnect: Vec::with_capacity(max_connections),
            registered: HashMap::with_capacity(max_connections),
            submission_errors: Vec::with_capacity(max_connections),
            raw_payloads_tx,
            submissions,
            submission_results,
        }
    }
}

impl Tile<HelixSpine> for BidSubmissionTcpListener {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        self.listener.poll_with(|event| match event {
            PollEvent::Accept { listener: _, stream, peer_addr } => {
                tracing::trace!("connected to new peer {:?} with token {:?}", peer_addr, stream);
            }
            PollEvent::Reconnect { token } => {
                tracing::trace!("reconnected to peer with token {:?}", token);
            }
            PollEvent::Disconnect { token } => {
                tracing::trace!("disconnected from peer with token {:?}", token);
                self.registered.remove(&token);
            }
            PollEvent::Message { token, payload, send_ts } => {
                if let Some(expected_pubkey) = self.registered.get(&token) {
                    let id = Uuid::new_v4();
                    if let Some(tx) = &self.raw_payloads_tx &&
                        tx.try_send((id, Bytes::copy_from_slice(payload))).is_err()
                    {
                        tracing::error!("s3 channel full, dropping payload");
                    }
                    match BidSubmission::try_from(payload).map_err(BidSubmissionError::from) {
                        Ok(bid) => {
                            let submission_ref = SubmissionRef::Tcp {
                                id,
                                token,
                                seq_num: bid.header.sequence_number,
                            };

                            let now = utcnow_ns();
                            SUB_CLIENT_TO_SERVER_LATENCY
                                .with_label_values(&["tcp"])
                                .observe((now.saturating_sub(send_ts.0) / 1000) as f64);

                            let trace = SubmissionTrace {
                                receive_ns: Nanos(now),
                                read_body_ns: Nanos(now),
                                ..Default::default()
                            };

                            let header =
                                InternalBidSubmissionHeader::from_tcp_header(id, bid.header);
                            let internal_bid = InternalBidSubmission {
                                header,
                                submission_ref,
                                trace,
                                body: bid.data,
                                span: Span::current(),
                                sent_at: Instant::now(),
                                expected_pubkey: Some(*expected_pubkey),
                            };
                            let ix = self.submissions.push(internal_bid);
                            adapter.produce(NewBidSubmissionIx { ix });
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

        adapter.consume(|SubmissionResultIx { ix }, _producers| {
            if let Some(submissione_result) = self.submission_results.get(ix) {
                match submissione_result.sub_ref {
                    SubmissionRef::Tcp { id, token, seq_num } => {
                        let response = response_from_builder_api_error(
                            seq_num,
                            id,
                            &submissione_result.result,
                        );
                        tracing::debug!("submission result: {}", response);
                        self.listener.write_or_enqueue_with(
                            SendBehavior::Single(token),
                            |buffer| {
                                response.ssz_append(buffer);
                            },
                        );
                    }
                    SubmissionRef::Http(_) => {}
                }
            }
        });
    }
}
