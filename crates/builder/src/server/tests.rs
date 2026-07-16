//! Loopback protocol test: a flux TCP client (acting exactly like the relay's
//! `BlockMergingTile`) against `MergingServerTile` backed by the `NoopEngine`.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use flux_network::tcp::{PollEvent, SendBehavior, TcpConnector, TcpTelemetry};
use helix_tcp_types::merging::{
    MERGING_HEADER_SIZE, MERGING_PROTOCOL_VERSION, MergingFrameHeader, MergingMsgId,
    builder_to_relay::{RejectCode, RejectV1},
    control::{MergerAckV1, MergerRegistrationV1, PingV1, PongV1},
    relay_to_builder::ActivateBaseBlockV1,
};
use ssz::Decode;

use super::MergingServerTile;
use crate::{config::MergingConfig, engine::NoopEngine, server::codec::append_frame};

const API_KEY: uuid::Uuid = uuid::Uuid::from_u128(1);

fn test_config(listen_addr: SocketAddr) -> MergingConfig {
    serde_yaml::from_str(&format!(
        "listen_addr: \"{listen_addr}\"\napi_keys: [\"{API_KEY}\"]\nhandshake_timeout_ms: 2000\n"
    ))
    .unwrap()
}

struct TestServer {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    fn start(addr: SocketAddr) -> Self {
        let config = test_config(addr);
        let (event_tx, event_rx) = crossbeam_channel::bounded(1024);
        let (output_tx, output_rx) = crossbeam_channel::bounded(64);
        NoopEngine::spawn(event_rx, output_tx);
        let mut tile = MergingServerTile::new(&config, event_tx, output_rx);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let handle = std::thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                tile.run_once();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        Self { stop, handle: Some(handle) }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct Client {
    connector: TcpConnector,
    token: flux_network::Token,
}

impl Client {
    fn connect(addr: SocketAddr) -> Self {
        let mut connector = TcpConnector::default().with_telemetry(TcpTelemetry::Disabled);
        let deadline = Instant::now() + Duration::from_secs(5);
        let token = loop {
            if let Some(token) = connector.connect(addr) {
                break token;
            }
            assert!(Instant::now() < deadline, "failed to dial test server");
            std::thread::sleep(Duration::from_millis(10));
        };
        Self { connector, token }
    }

    fn send(&mut self, msg_id: MergingMsgId, msg: &impl ssz::Encode) {
        let token = self.token;
        self.connector.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
            append_frame(buf, msg_id, msg);
        });
    }

    fn send_raw(&mut self, frame: &[u8]) {
        let token = self.token;
        self.connector.write_or_enqueue_with(SendBehavior::Single(token), |buf| {
            buf.extend_from_slice(frame);
        });
    }

    /// Polls until a full frame arrives or the timeout expires.
    fn recv(&mut self, timeout: Duration) -> Option<(MergingMsgId, Vec<u8>)> {
        let deadline = Instant::now() + timeout;
        let mut received = None;
        let mut disconnected = false;
        while Instant::now() < deadline && received.is_none() && !disconnected {
            self.connector.poll_with(|event| match event {
                PollEvent::Message { payload, .. } => {
                    let header = MergingFrameHeader::decode(payload).expect("bad header");
                    received = Some((header.msg_id, payload[MERGING_HEADER_SIZE..].to_vec()));
                }
                PollEvent::Disconnect { .. } => disconnected = true,
                _ => {}
            });
            if received.is_none() {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        received
    }

    /// Polls until the server drops the connection.
    fn wait_disconnect(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut disconnected = false;
        while Instant::now() < deadline && !disconnected {
            self.connector.poll_with(|event| {
                if matches!(event, PollEvent::Disconnect { .. } | PollEvent::Reconnect { .. }) {
                    disconnected = true;
                }
            });
            std::thread::sleep(Duration::from_millis(2));
        }
        disconnected
    }

    fn register(&mut self, api_key: uuid::Uuid) {
        let msg = MergerRegistrationV1 {
            api_key: api_key.into_bytes(),
            relay_id: b"test-relay".to_vec(),
            min_version: MERGING_PROTOCOL_VERSION,
            max_version: MERGING_PROTOCOL_VERSION,
            supports_zstd: false,
        };
        self.send(MergingMsgId::MergerRegistrationV1, &msg);
    }
}

/// Unique-ish port per test to avoid collisions in the parallel test runner.
fn test_addr(offset: u16) -> SocketAddr {
    let base = 23800 + (std::process::id() % 500) as u16;
    format!("127.0.0.1:{}", base + offset).parse().unwrap()
}

#[test]
fn handshake_ping_and_reject_flow() {
    let addr = test_addr(0);
    let _server = TestServer::start(addr);

    let mut client = Client::connect(addr);
    client.register(API_KEY);

    let (msg_id, body) = client.recv(Duration::from_secs(5)).expect("no ack");
    assert_eq!(msg_id, MergingMsgId::MergerAckV1);
    let ack = MergerAckV1::from_ssz_bytes(&body).unwrap();
    assert!(ack.status.is_okay());
    assert_eq!(ack.version, MERGING_PROTOCOL_VERSION);
    assert!(ack.max_orders_per_slot > 0);

    // Ping -> pong.
    client.send(MergingMsgId::PingV1, &PingV1 { nonce: 42 });
    let (msg_id, body) = client.recv(Duration::from_secs(5)).expect("no pong");
    assert_eq!(msg_id, MergingMsgId::PongV1);
    assert_eq!(PongV1::from_ssz_bytes(&body).unwrap().nonce, 42);

    // Extension ids (>= 0x80) must be ignored.
    client.send_raw(&[0x80, 0, 1, 2, 3]);

    // The noop engine rejects every activation with UnknownBaseBlock.
    let activate =
        ActivateBaseBlockV1 { slot: 7, block_hash: alloy_primitives::B256::repeat_byte(0xaa) };
    client.send(MergingMsgId::ActivateBaseBlockV1, &activate);
    let (msg_id, body) = client.recv(Duration::from_secs(5)).expect("no reject");
    assert_eq!(msg_id, MergingMsgId::RejectV1);
    let reject = RejectV1::from_ssz_bytes(&body).unwrap();
    assert_eq!(reject.slot, 7);
    assert_eq!(reject.code, RejectCode::UnknownBaseBlock);
}

#[test]
fn bad_api_key_is_rejected_and_disconnected() {
    let addr = test_addr(1);
    let _server = TestServer::start(addr);

    let mut client = Client::connect(addr);
    client.register(uuid::Uuid::from_u128(0xdead));

    let (msg_id, body) = client.recv(Duration::from_secs(5)).expect("no ack");
    assert_eq!(msg_id, MergingMsgId::MergerAckV1);
    let ack = MergerAckV1::from_ssz_bytes(&body).unwrap();
    assert!(ack.status.is_err());
    assert!(client.wait_disconnect(Duration::from_secs(5)), "expected disconnect");
}

#[test]
fn version_mismatch_is_rejected() {
    let addr = test_addr(2);
    let _server = TestServer::start(addr);

    let mut client = Client::connect(addr);
    let msg = MergerRegistrationV1 {
        api_key: API_KEY.into_bytes(),
        relay_id: b"test-relay".to_vec(),
        min_version: 99,
        max_version: 100,
        supports_zstd: false,
    };
    client.send(MergingMsgId::MergerRegistrationV1, &msg);

    let (msg_id, body) = client.recv(Duration::from_secs(5)).expect("no ack");
    assert_eq!(msg_id, MergingMsgId::MergerAckV1);
    assert!(MergerAckV1::from_ssz_bytes(&body).unwrap().status.is_err());
}

#[test]
fn garbage_frame_before_registration_disconnects() {
    let addr = test_addr(3);
    let _server = TestServer::start(addr);

    let mut client = Client::connect(addr);
    client.send_raw(&[0x7f, 0, 1, 2, 3]);
    assert!(client.wait_disconnect(Duration::from_secs(5)), "expected disconnect");
}
