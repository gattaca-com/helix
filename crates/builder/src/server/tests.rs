//! Loopback protocol test: a flux TCP client (acting exactly like the relay's
//! `BlockMergingTile`) against `MergingServerTile` on a spine with no engine attached.

use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use flux::{spine::SpineAdapter, tile::Tile};
use flux_network::{Event, Network, TcpGroupConfig, Token};
use helix_tcp_types::merging::{
    MERGING_HEADER_SIZE, MERGING_PROTOCOL_VERSION, MergingFrameHeader, MergingMsgId,
    builder_to_relay::{RejectCode, RejectV1},
    control::{MergerAckV1, MergerRegistrationV1, PingV1, PongV1},
};
use ssz::Decode;

use super::{CONTROL_HEADROOM, MergingServerTile};
use crate::{
    config::MergingConfig,
    server::codec::{append_body_frame, append_frame},
    spine::{EngineCursor, charge},
    testing::TestSpine,
};

const API_KEY: uuid::Uuid = uuid::Uuid::from_u128(1);

fn test_config(listen_addr: SocketAddr) -> MergingConfig {
    serde_yaml::from_str(&format!(
        "listen_addr: \"{listen_addr}\"\napi_keys: [\"{API_KEY}\"]\nhandshake_timeout_ms: 2000\n"
    ))
    .unwrap()
}

struct TestServer {
    /// No engine runs: tests advance the credit themselves.
    cursor: Arc<EngineCursor>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    _spine: TestSpine,
}

impl TestServer {
    fn start(addr: SocketAddr) -> Self {
        let config = test_config(addr);
        let mut spine = TestSpine::new();
        let cursor = Arc::new(EngineCursor::default());
        let mut tile = MergingServerTile::new(&config, cursor.clone());
        let mut adapter = SpineAdapter::connect_tile(&tile, &mut spine.spine);
        tile.on_attach(&mut adapter);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let handle = std::thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                tile.run_once(&mut adapter);
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        Self { cursor, stop, handle: Some(handle), _spine: spine }
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
    connector: Network,
    token: Token,
    /// Sticky: a `recv` that sees the reply and the close in one poll must
    /// not hide the close from `wait_disconnect`.
    disconnected: bool,
    /// Frames polled but not yet returned; one poll can deliver several.
    pending: VecDeque<(MergingMsgId, Vec<u8>)>,
}

impl Client {
    fn connect(addr: SocketAddr) -> Self {
        let mut connector = Network::default();
        let group = connector.add_group(TcpGroupConfig::default());
        let token = connector.connect(group, addr);
        Self { connector, token, disconnected: false, pending: VecDeque::new() }
    }

    fn send(&mut self, msg_id: MergingMsgId, msg: &impl ssz::Encode) {
        let mut frame = Vec::new();
        append_frame(&mut frame, msg_id, msg);
        self.send_raw(&frame);
    }

    fn send_raw(&mut self, frame: &[u8]) {
        self.connector.send_with(self.token, |buf| buf.extend_from_slice(frame));
    }

    /// Polls until a full frame arrives or the timeout expires.
    fn recv(&mut self, timeout: Duration) -> Option<(MergingMsgId, Vec<u8>)> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline && self.pending.is_empty() && !self.disconnected {
            self.connector.poll_with(|event| match event {
                Event::Message { payload, .. } => {
                    let header = MergingFrameHeader::decode(payload).expect("bad header");
                    let body = payload[MERGING_HEADER_SIZE..].to_vec();
                    self.pending.push_back((header.msg_id, body));
                }
                Event::Disconnected { .. } => self.disconnected = true,
                _ => {}
            });
            if self.pending.is_empty() {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        self.pending.pop_front()
    }

    /// `recv`, skipping the server's pings.
    fn recv_reply(&mut self, timeout: Duration) -> Option<(MergingMsgId, Vec<u8>)> {
        loop {
            match self.recv(timeout) {
                Some((MergingMsgId::PingV1, _)) => {}
                other => return other,
            }
        }
    }

    /// Polls until the server drops the connection.
    fn wait_disconnect(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline && !self.disconnected {
            self.connector.poll_with(|event| {
                if matches!(event, Event::Disconnected { .. }) {
                    self.disconnected = true;
                }
            });
            std::thread::sleep(Duration::from_millis(2));
        }
        self.disconnected
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

/// With the engine behind, blocks past the queue's credit are refused with `Busy` and control
/// headroom is kept; returned credit admits blocks again.
#[test]
fn engine_queue_full_rejects_busy_until_credit_returns() {
    let addr = test_addr(4);
    let server = TestServer::start(addr);
    let mut client = Client::connect(addr);
    client.register(API_KEY);
    let (msg_id, _) = client.recv(Duration::from_secs(5)).expect("no ack");
    assert_eq!(msg_id, MergingMsgId::MergerAckV1);

    let body = [0u8; 16];
    let mut frame = Vec::new();
    append_body_frame(&mut frame, MergingMsgId::MergeableBlockV1, &body, false);
    // TestSpine's to_engine depth; registration's ConnectionReset holds one event.
    let depth = 64;
    let admitted = depth - CONTROL_HEADROOM.0 - 1;
    let sent = depth + 8;
    for _ in 0..sent {
        client.send_raw(&frame);
    }
    for _ in admitted..sent {
        let (msg_id, reply) = client.recv_reply(Duration::from_secs(5)).expect("no reject");
        assert_eq!(msg_id, MergingMsgId::RejectV1);
        assert_eq!(RejectV1::from_ssz_bytes(&reply).unwrap().code, RejectCode::Busy);
    }
    assert!(client.recv_reply(Duration::from_millis(200)).is_none(), "admitted blocks rejected");

    server.cursor.charge.store(admitted * charge(body.len()), Ordering::Release);
    server.cursor.seq.store(admitted + 1, Ordering::Release);
    client.send_raw(&frame);
    assert!(client.recv_reply(Duration::from_millis(200)).is_none(), "credit not returned");
}

/// The server stamps `response_id` into the engine's encoding in place.
#[test]
fn merged_response_id_offset_matches_the_encoding() {
    use alloy_rpc_types::engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
    use helix_tcp_types::merging::builder_to_relay::MergedBlockV1;
    use ssz::Encode;

    let v1 = ExecutionPayloadV1 {
        parent_hash: Default::default(),
        fee_recipient: Default::default(),
        state_root: Default::default(),
        receipts_root: Default::default(),
        logs_bloom: Default::default(),
        prev_randao: Default::default(),
        block_number: 0,
        gas_limit: 0,
        gas_used: 0,
        timestamp: 0,
        extra_data: Default::default(),
        base_fee_per_gas: Default::default(),
        block_hash: Default::default(),
        transactions: vec![],
    };
    let v2 = ExecutionPayloadV2 { payload_inner: v1, withdrawals: vec![] };
    let execution_payload =
        ExecutionPayloadV3 { payload_inner: v2, blob_gas_used: 0, excess_blob_gas: 0 };
    let msg = MergedBlockV1 {
        slot: u64::MAX,
        response_id: 0xa1b2_c3d4,
        base_block_hash: Default::default(),
        base_builder_pubkey: Default::default(),
        execution_payload,
        execution_requests: Default::default(),
        appended_blobs: vec![],
        proposer_value: Default::default(),
        base_builder_revenue: Default::default(),
        relay_revenue: Default::default(),
        builder_inclusions: vec![],
        included_order_ids: vec![],
        trace: Default::default(),
        unmerged_txs: vec![],
    };
    let body = msg.as_ssz_bytes();
    assert_eq!(body[super::MERGED_RESPONSE_ID], 0xa1b2_c3d4u32.to_le_bytes());
}
