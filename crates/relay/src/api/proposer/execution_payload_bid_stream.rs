use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use axum::{
    Extension,
    extract::{
        Path,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use helix_common::{
    api::{
        HEADER_REQUEST_AUTH,
        proposer_api::{GetExecutionPayloadBidParams, GetHeaderParams},
    },
    api_provider::ApiProvider,
    utils::extract_request_id,
};
use helix_types::{ForkName, SignedBuilderRequestAuth, gloas};
use http::HeaderMap;
use ssz::Decode;
use tokio::time::Instant;
use tracing::{Instrument, debug, error, info, trace, warn};

use super::{
    ProposerApi,
    error::ProposerApiError,
    get_execution_payload_bid::verify_request_auth,
    get_header::ValidatedHeaderRequest,
    header_stream::{ProposerStream, SendResult, StreamSchedule, build_frame},
};
use crate::{
    api::{Api, router::Terminating},
    auctioneer::BidKey,
};

/// Base64 of the largest valid ssz `SignedBuilderRequestAuth`: 112 fixed bytes and 4096 of data.
const MAX_REQUEST_AUTH_HEADER_LEN: usize = (112 + 4096usize).div_ceil(3) * 4;
const CLOSE_TIMEOUT: Duration = Duration::from_millis(100);

impl<A: Api> ProposerApi<A> {
    /// Gloas `getExecutionPayloadBid` as a websocket stream, scheduled as `header_stream`.
    /// With no body, the request auth comes in [`HEADER_REQUEST_AUTH`].
    #[tracing::instrument(skip_all, err(level = tracing::Level::TRACE), fields(id =% extract_request_id(&headers), slot = params.slot, parent_hash =? params.parent_hash, parent_root =? params.parent_root))]
    pub async fn execution_payload_bid_stream(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        Extension(Terminating(terminating)): Extension<Terminating>,
        headers: HeaderMap,
        Path(params): Path<GetExecutionPayloadBidParams>,
        ws: WebSocketUpgrade,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        trace!("starting call");

        if proposer_api.chain_info.fork_at_slot(params.slot.into()) != ForkName::Gloas {
            return Err(ProposerApiError::InvalidFork);
        }

        let config = &proposer_api.relay_config.header_stream;
        let header_params = GetHeaderParams {
            slot: params.slot,
            parent_hash: params.parent_hash,
            pubkey: params.proposer_pubkey,
        };

        let ValidatedHeaderRequest {
            ms_into_slot,
            is_mev_boost,
            timeout_ms,
            preferences,
            proposer_pubkey,
            ..
        } = proposer_api.validate_header_request(&header_params, &headers, &terminating)?;

        if params.proposer_pubkey != proposer_pubkey {
            return Err(ProposerApiError::UnexpectedProposerPubkey {
                expected: proposer_pubkey,
                actual: params.proposer_pubkey,
            });
        }

        proposer_api
            .api_provider
            .admit_header_stream(&header_params, &headers, &preferences, config)
            .map_err(|reason| {
                warn!(slot = params.slot, proposer = %params.proposer_pubkey, reason, "refusing bid stream");
                ProposerApiError::StreamNotAdmitted
            })?;

        let Some(timeout_ms) = timeout_ms else {
            return Err(ProposerApiError::InvalidGetHeader("missing or invalid x-timeout-ms"));
        };

        let auth = headers
            .get(HEADER_REQUEST_AUTH)
            .ok_or(ProposerApiError::InvalidRequestAuthHeader("missing"))?;
        if auth.len() > MAX_REQUEST_AUTH_HEADER_LEN {
            return Err(ProposerApiError::InvalidRequestAuthHeader("too long"));
        }
        let auth = STANDARD
            .decode(auth.as_bytes())
            .map_err(|_| ProposerApiError::InvalidRequestAuthHeader("not base64"))?;
        let auth = SignedBuilderRequestAuth::from_ssz_bytes(&auth)
            .map_err(|_| ProposerApiError::InvalidRequestAuthHeader("not an ssz request auth"))?;

        // Last, so a request the checks above refuse never costs a BLS verify
        verify_request_auth(&auth, &params, proposer_api.chain_info.request_auth_domain)?;

        let now = Instant::now();
        let schedule = StreamSchedule::new(now, &headers, timeout_ms, ms_into_slot, config);

        let (auth_slot, auth_data_len, auth_data) = super::auth_summary(&auth);
        info!(
            ms_into_slot,
            is_mev_boost,
            timeout_ms,
            handshake_ms = schedule.handshake_ms,
            auth_slot,
            auth_data_len,
            %auth_data,
            start_in_ms = (schedule.start - now).as_millis(),
            window_ms = (schedule.end - schedule.start).as_millis(),
            "accepting bid stream"
        );

        let stream = ExecutionPayloadBidStream { api: proposer_api, terminating, params, schedule };

        Ok(ws
            .on_failed_upgrade(|error| warn!(%error, "bid stream upgrade failed"))
            .on_upgrade(move |socket| stream.run(socket).in_current_span()))
    }
}

/// Per-connection state, moved into the task that owns the upgraded socket.
struct ExecutionPayloadBidStream<A: Api> {
    api: Arc<ProposerApi<A>>,
    terminating: Arc<AtomicBool>,
    params: GetExecutionPayloadBidParams,
    schedule: StreamSchedule,
}

impl<A: Api> ExecutionPayloadBidStream<A> {
    async fn run(self, mut socket: WebSocket) {
        let (delivered, hit_deadline) =
            self.schedule.run(&mut socket, &self.terminating, &self).await;
        // A client cannot tell a dropped socket from a failed stream. Bounded, so a peer
        // that stopped reading cannot hold the task
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, socket.send(Message::Close(None))).await;

        info!(delivered, hit_deadline, "bid stream closed");
    }
}

impl<A: Api> ProposerStream for ExecutionPayloadBidStream<A> {
    type Sent = BidKey;

    async fn send_bid(&self, socket: &mut WebSocket, last_sent: &mut Option<BidKey>) -> SendResult {
        let Ok(rx) = self.api.auctioneer_handle.get_execution_payload_bid(self.params, *last_sent)
        else {
            error!("failed to send get_execution_payload_bid to auctioneer");
            return SendResult::Skipped;
        };

        let signed_bid = match rx.await {
            Ok(Ok(signed_bid)) => signed_bid,
            Ok(Err(ProposerApiError::BidUnchanged)) => return SendResult::Skipped,
            Ok(Err(err)) if err.ends_bid_stream() => {
                debug!(%err, "ending bid stream");
                return SendResult::Closed;
            }
            Ok(Err(err)) => {
                trace!(%err, "no bid to stream");
                return SendResult::Skipped;
            }
            Err(err) => {
                warn!(%err, "failed to get execution payload bid from auctioneer");
                return SendResult::Skipped;
            }
        };

        let sent = BidKey::of(gloas::signed_bid_message(&signed_bid));
        *last_sent = Some(sent);

        let frame = build_frame(ForkName::Gloas, &signed_bid);
        if let Err(err) = socket.send(Message::binary(frame)).await {
            debug!(%err, "failed to send bid");
            return SendResult::Closed;
        }

        info!(
            block_hash = %sent.block_hash,
            execution_payment = sent.execution_payment,
            "streamed bid"
        );

        SendResult::Bid
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use alloy_primitives::B256;
    use axum::{Router, routing::get};
    use flux::{
        spine::{FluxSpine, StandaloneProducer},
        tile::TileName,
    };
    use futures::StreamExt;
    use helix_common::{
        CurrentSlotInfo, HeaderStreamConfig, RelayConfig, Route, SlotDuties,
        alerts::AlertManager,
        api::{HEADER_TIMEOUT_MS, builder_api::BuilderGetValidatorsResponseEntry},
        api_provider::DefaultApiProvider,
        beacon::MultiBeaconClient,
        chain_info::ChainInfo,
        local_cache::LocalCache,
        signing::RelaySigningContext,
        utils::utcnow_sec,
    };
    use helix_database::handle::DbHandle;
    use helix_types::{
        BlsKeypair, BlsPublicKeyBytes, BlsSignature, BuilderRequestAuth, ChainSpec, Epoch,
        ExecutionBlockHash, ExecutionPayloadBid, RequestAuthData, SignedExecutionPayloadBid, Slot,
    };
    use http::HeaderValue;
    use ssz::Encode;
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{self, Message as Frame, client::IntoClientRequest},
    };
    use tree_hash::TreeHash;

    use super::*;
    use crate::{
        HelixSpine,
        auctioneer::{AuctioneerHandle, Event},
        gossip::GrpcGossiperClientManager,
        registration::RegWorkerHandle,
        spine::messages::GetHeaderServed,
    };

    #[derive(Clone)]
    struct TestApi;

    impl Api for TestApi {
        type ApiProvider = DefaultApiProvider;
    }

    /// Gloas from epoch 10, so `SLOT` is a Gloas slot and `PRE_GLOAS_SLOT` is not.
    const GLOAS_EPOCH: u64 = 10;
    const SLOT: u64 = 1000;
    const PRE_GLOAS_SLOT: u64 = 100;

    /// A producer on a spine of its own, in a directory no other test uses.
    fn header_served() -> StandaloneProducer<GetHeaderServed> {
        static SPINES: AtomicUsize = AtomicUsize::new(0);
        let n = SPINES.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("helix-bid-stream-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut spine = HelixSpine::new_with_base_dir(&dir, Some("bid-stream-test"));
        spine.standalone_producer_for(TileName::from_str_truncate("ProposerApi"))
    }

    struct Relay {
        url: String,
        api: Arc<ProposerApi<TestApi>>,
        proposer: BlsKeypair,
        /// `last_sent` of every auctioneer request, in order.
        polls: Arc<Mutex<Vec<Option<BidKey>>>>,
    }

    /// A relay serving the bid stream for `SLOT`, whose auctioneer answers the n-th request
    /// with `answer(n, last_sent)`.
    async fn relay(
        admit_all: bool,
        answer: impl Fn(usize, Option<BidKey>) -> Result<Vec<u8>, ProposerApiError> + Send + 'static,
    ) -> Relay {
        let mut spec = ChainSpec::mainnet();
        spec.gloas_fork_epoch = Some(Epoch::new(GLOAS_EPOCH));
        // Genesis now puts `SLOT` in the future, so no request for it is late
        let chain_info = Arc::new(ChainInfo::new(spec, B256::ZERO, utcnow_sec()));

        let proposer = BlsKeypair::random();
        let mut duty = BuilderGetValidatorsResponseEntry::default();
        duty.entry.registration.message.pubkey = proposer.pk.serialize().into();
        let slot_info = CurrentSlotInfo::new();
        slot_info.handle_new_slot(
            SlotDuties { slot: Slot::new(SLOT - 1), next_duty: Some(duty), new_duties: None },
            &chain_info,
        );

        let mut config = RelayConfig::empty_for_test();
        // Longer than any window here, so polling starts when the stream does
        config.header_stream =
            HeaderStreamConfig { stream_for_ms: 10_000, interval_ms: 5, admit_all };

        let (auctioneer_tx, auctioneer_rx) = crossbeam_channel::unbounded();
        let polls = Arc::new(Mutex::new(Vec::new()));
        let seen = polls.clone();
        std::thread::spawn(move || {
            while let Ok(Event::GetExecutionPayloadBid { last_sent, res_tx, .. }) =
                auctioneer_rx.recv()
            {
                let n = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(last_sent);
                    seen.len() - 1
                };
                let _ = res_tx.send(answer(n, last_sent));
            }
        });

        let api = Arc::new(ProposerApi::<TestApi>::new(
            Arc::new(LocalCache::default()),
            DbHandle::new(crossbeam_channel::unbounded().0, crossbeam_channel::unbounded().0),
            Arc::new(GrpcGossiperClientManager::new(vec![]).await.unwrap()),
            Arc::new(DefaultApiProvider),
            Arc::new(RelaySigningContext::default()),
            Arc::new(MultiBeaconClient::new(vec![])),
            chain_info,
            Default::default(),
            config,
            slot_info,
            AuctioneerHandle::new(auctioneer_tx),
            RegWorkerHandle::new(crossbeam_channel::unbounded().0),
            Arc::new(AlertManager::Disabled),
            None,
            header_served(),
        ));

        let router = Router::new()
            .route(
                &Route::ExecutionPayloadBidStream.path(),
                get(ProposerApi::<TestApi>::execution_payload_bid_stream),
            )
            .layer(Extension(api.clone()))
            .layer(Extension(Terminating(Arc::new(AtomicBool::new(false)))));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });

        Relay { url, api, proposer, polls }
    }

    impl Relay {
        fn request(
            &self,
            slot: u64,
            pubkey: BlsPublicKeyBytes,
            auth: Option<&str>,
            timeout_ms: u64,
        ) -> tungstenite::handshake::client::Request {
            let mut request = format!(
                "{}/eth/v1/builder/execution_payload_bid_stream/{slot}/{}/{}/{pubkey}",
                self.url,
                B256::ZERO,
                B256::ZERO
            )
            .into_client_request()
            .unwrap();
            let headers = request.headers_mut();
            headers.insert(HEADER_TIMEOUT_MS, HeaderValue::from(timeout_ms));
            if let Some(auth) = auth {
                headers.insert(HEADER_REQUEST_AUTH, HeaderValue::from_str(auth).unwrap());
            }
            request
        }

        fn proposer_pubkey(&self) -> BlsPublicKeyBytes {
            self.proposer.pk.serialize().into()
        }

        /// The largest auth there is, encoded so that it decodes only as standard padded
        /// base64: it carries `+`, `/` and padding.
        fn auth(&self, signer: &BlsKeypair) -> String {
            // The data starts at byte 112 of the ssz, so bytes 2..5 encode as "++++"
            let mut data = vec![0xff; 4096];
            data[2..5].copy_from_slice(&[0xfb, 0xef, 0xbe]);
            let message = BuilderRequestAuth { data: RequestAuthData(data.into()), slot: SLOT };
            let signing_root = gloas::signing_root(
                message.tree_hash_root(),
                self.api.chain_info.request_auth_domain,
            );
            let signature = signer.sk.sign(signing_root).serialize().into();
            let auth =
                STANDARD.encode(SignedBuilderRequestAuth { message, signature }.as_ssz_bytes());
            assert!(auth.contains('+') && auth.contains('/') && auth.ends_with('='));
            auth
        }
    }

    #[tokio::test]
    async fn each_new_bid_is_sent_once_and_the_stream_ends_with_a_close_frame() {
        let signed_bid = |block_hash: u8, execution_payment| {
            SignedExecutionPayloadBid {
                message: ExecutionPayloadBid {
                    block_hash: ExecutionBlockHash(B256::repeat_byte(block_hash)),
                    execution_payment,
                    slot: Slot::new(SLOT),
                    ..Default::default()
                },
                signature: BlsSignature::empty(),
            }
            .as_ssz_bytes()
        };
        let first = signed_bid(0x99, 42);
        let improved = signed_bid(0xaa, 43);
        let first_key = BidKey { block_hash: B256::repeat_byte(0x99), execution_payment: 42 };
        let improved_key = BidKey { block_hash: B256::repeat_byte(0xaa), execution_payment: 43 };
        let relay = relay(true, {
            let (first, improved) = (first.clone(), improved.clone());
            move |n, last_sent| {
                let (top, key) =
                    if n < 3 { (&first, first_key) } else { (&improved, improved_key) };
                if last_sent == Some(key) {
                    Err(ProposerApiError::BidUnchanged)
                } else {
                    Ok(top.clone())
                }
            }
        })
        .await;
        let auth = relay.auth(&relay.proposer);

        let request = relay.request(SLOT, relay.proposer_pubkey(), Some(&auth), 100);
        let (mut socket, _) = connect_async(request).await.unwrap();
        let mut frames = Vec::new();
        let last = loop {
            match socket.next().await {
                Some(Ok(Frame::Binary(frame))) => frames.push(frame),
                last => break last,
            }
        };

        assert!(matches!(last, Some(Ok(Frame::Close(_)))), "got {last:?}");
        let polls = relay.polls.lock().unwrap().clone();
        assert!(polls.len() > 4, "polled {} times", polls.len());
        assert!(polls[0].is_none());
        assert!(polls[1..4].iter().all(|sent| *sent == Some(first_key)), "{polls:?}");
        assert!(polls[4..].iter().all(|sent| *sent == Some(improved_key)), "{polls:?}");
        assert_eq!(frames.len(), 2, "an unchanged bid is not sent again");
        for (frame, bid) in frames.iter().zip([first, improved]) {
            assert_eq!(frame[..2], [1, 7]);
            assert_eq!(frame[2..], bid[..], "the auctioneer's signed bid, as it is");
        }
    }

    #[tokio::test]
    async fn the_stream_ends_once_the_auction_has_moved_past_it() {
        let relay = relay(true, |n, _| {
            Err(match n {
                0 => ProposerApiError::NoBidPrepared,
                // The auctioneer has yet to reach the requested slot
                1 => ProposerApiError::RequestWrongSlot { request_slot: SLOT, bid_slot: SLOT - 1 },
                _ => ProposerApiError::RequestWrongSlot { request_slot: SLOT, bid_slot: SLOT + 1 },
            })
        })
        .await;
        let auth = relay.auth(&relay.proposer);

        let request = relay.request(SLOT, relay.proposer_pubkey(), Some(&auth), 3000);
        let (mut socket, _) = connect_async(request).await.unwrap();
        let last = socket.next().await;

        assert!(matches!(last, Some(Ok(Frame::Close(_)))), "got {last:?}");
        assert_eq!(relay.polls.lock().unwrap().len(), 3);
    }

    /// Each refusal is an HTTP status, so no socket opens. The cheap checks come first, and the
    /// BLS verify last.
    #[tokio::test]
    async fn a_refused_stream_is_answered_before_the_upgrade() {
        let not_admitting = relay(false, |_, _| Err(ProposerApiError::NoBidPrepared)).await;
        let relay = relay(true, |_, _| Err(ProposerApiError::NoBidPrepared)).await;
        let proposer = relay.proposer_pubkey();
        let other = BlsKeypair::random();
        let other_pubkey: BlsPublicKeyBytes = other.pk.serialize().into();
        let valid = relay.auth(&relay.proposer);
        let signed_by_other = relay.auth(&other);
        let too_long = "A".repeat(MAX_REQUEST_AUTH_HEADER_LEN + 4);
        let not_ssz = STANDARD.encode([1u8, 2, 3]);

        let cases = [
            (&relay, PRE_GLOAS_SLOT, proposer, Some(valid.as_str()), ProposerApiError::InvalidFork),
            // Refused before its auth is even decoded
            (
                &relay,
                SLOT,
                other_pubkey,
                Some("not*base64"),
                ProposerApiError::UnexpectedProposerPubkey {
                    expected: proposer,
                    actual: other_pubkey,
                },
            ),
            // Also signed by another key: admission is refused before the BLS verify
            (
                &not_admitting,
                SLOT,
                not_admitting.proposer_pubkey(),
                Some(&signed_by_other),
                ProposerApiError::StreamNotAdmitted,
            ),
            (&relay, SLOT, proposer, None, ProposerApiError::InvalidRequestAuthHeader("missing")),
            (
                &relay,
                SLOT,
                proposer,
                Some(&too_long),
                ProposerApiError::InvalidRequestAuthHeader("too long"),
            ),
            (
                &relay,
                SLOT,
                proposer,
                Some("not*base64"),
                ProposerApiError::InvalidRequestAuthHeader("not base64"),
            ),
            (
                &relay,
                SLOT,
                proposer,
                Some(&not_ssz),
                ProposerApiError::InvalidRequestAuthHeader("not an ssz request auth"),
            ),
            (
                &relay,
                SLOT,
                proposer,
                Some(&signed_by_other),
                ProposerApiError::InvalidRequestAuthSignature,
            ),
        ];

        for (relay, slot, pubkey, auth, refusal) in cases {
            let expected = refusal.to_string();
            let status = refusal.into_response().status();

            let result = connect_async(relay.request(slot, pubkey, auth, 1000)).await;

            let Err(tungstenite::Error::Http(response)) = result else {
                panic!("expected {expected}, got {result:?}");
            };
            assert_eq!(response.status(), status, "{expected}");
            let body = response.body().as_deref().unwrap_or_default();
            assert_eq!(String::from_utf8_lossy(body), expected);
        }
    }
}
