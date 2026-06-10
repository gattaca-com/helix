//! Example relay-side client for the builder block-merging TCP protocol
//! (`doc/block-merging.md` in the builder repo). Connects to a builder
//! running with `block_merging_builder: true` and walks through a full
//! interaction:
//!
//!   registration -> ack -> relay config -> slot start -> ping/pong
//!   -> mergeable block (payload + order refs) -> activate
//!   -> stream of merged blocks / rejects
//!
//! The client is intentionally self-contained: it implements the frame
//! layout (`[u32 LE len][u64 LE send-ts][1B msg_type][1B flags][SSZ body]`)
//! directly, so it doubles as a reference for the relay-side implementation.
//!
//! The base block and orders carry validly-signed txs from throwaway keys, so
//! they pass the builder's wire validation. Against a live builder the
//! fabricated base block is then rejected at the head check
//! (`HeadMismatch`/`NotSynced`) — that reject round-trip is part of the demo.
//! Pass the builder's real slot/block number to get past it.
//!
//! Usage:
//!   cargo run -p helix-tcp-types --example merging_relay_client -- \
//!       <addr> <api-key-uuid> [slot] [block-number]

use std::time::{SystemTime, UNIX_EPOCH};

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bloom, Bytes, TxKind, U256};
use alloy_rpc_types::{
    beacon::BlsPublicKey,
    engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3},
};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use helix_tcp_types::merging::{
    MERGING_PROTOCOL_VERSION, MergingFrameHeader, MergingMsgId,
    builder_to_relay::{FatalV1, MergedBlockV1, RejectV1},
    control::{
        BuilderCollateral, MergerAckV1, MergerRegistrationV1, PingV1, PongV1, RelayConfigV1,
    },
    order::{BundleOrderRef, MergeOrderRef, TxOrderRef},
    relay_to_builder::{ActivateBaseBlockV1, MergeableBlockV1, SlotStartV1},
};
use ssz::{Decode, Encode};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::Duration,
};

const FRAME_HEADER_SIZE: usize = 12; // u32 LE payload len + u64 LE send-ts nanos
const CHAIN_ID: u64 = 1;
const GWEI: u128 = 1_000_000_000;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| usage());
    let api_key =
        args.next().and_then(|s| uuid::Uuid::parse_str(&s).ok()).unwrap_or_else(|| usage());
    let slot: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let block_number: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);

    let mut stream = TcpStream::connect(&addr).await?;
    stream.set_nodelay(true)?;
    println!("connected to {addr}");

    // -- 1. Registration. Must be the first frame; the builder validates the
    //       api key against its allowlist and negotiates the version.
    write_msg(&mut stream, MergingMsgId::MergerRegistrationV1, &MergerRegistrationV1 {
        api_key: *api_key.as_bytes(),
        relay_id: b"example-relay".to_vec(),
        min_version: MERGING_PROTOCOL_VERSION,
        max_version: MERGING_PROTOCOL_VERSION,
        supports_zstd: false,
    })
    .await?;

    let (msg_id, body) = read_msg(&mut stream).await?;
    assert_eq!(msg_id, MergingMsgId::MergerAckV1, "expected ack, got {msg_id:?}");
    let ack = MergerAckV1::from_ssz_bytes(&body).expect("ack decode");
    println!(
        "ack: version={} status={:?} max_orders_per_slot={} max_frame_bytes={}",
        ack.version, ack.status, ack.max_orders_per_slot, ack.max_frame_bytes
    );
    if ack.status.is_err() {
        println!("registration rejected: {}", String::from_utf8_lossy(&ack.error_msg));
        return Ok(());
    }

    // Demo identities. The base-block fee recipient must appear in the
    // collateral map or the builder rejects with UnknownCollateral.
    let winning_builder_coinbase = Address::random();
    let collateral_safe = Address::random();
    let proposer_fee_recipient = Address::random();
    let parent_beacon_block_root = B256::random();
    let parent_hash = B256::random();

    // -- 2. Relay-owned distribution policy. Re-sendable; takes effect at the
    //       next slot start.
    write_msg(&mut stream, MergingMsgId::RelayConfigV1, &RelayConfigV1 {
        relay_fee_recipient: Address::random(),
        multisend_contract: Address::random(),
        relay_bps: 2500,
        merged_builder_bps: 2500,
        winning_builder_bps: 2500,
        distribution_gas_limit: 140_000,
        builder_collaterals: vec![BuilderCollateral {
            builder_coinbase: winning_builder_coinbase,
            collateral_safe,
        }],
    })
    .await?;

    // -- 3. Slot boundary: flushes all builder-side merging state for older
    //       slots.
    write_msg(&mut stream, MergingMsgId::SlotStartV1, &SlotStartV1 {
        slot,
        parent_hash,
        proposer_fee_recipient,
        parent_beacon_block_root,
    })
    .await?;

    // -- 4. Liveness check.
    write_msg(&mut stream, MergingMsgId::PingV1, &PingV1 { nonce: 7 }).await?;

    // -- 5. Forward a mergeable block: a builder submission whose payload
    //       carries both the mergeable txs (referenced by index) and, since
    //       `allow_appending` is set, serves as a merge-base candidate. The
    //       last tx must be the proposer payment of exactly `block_value`.
    let order_tx = signed_tx_bytes(&PrivateKeySigner::random(), 0, Address::random(), 1);
    let searcher = PrivateKeySigner::random();
    let bundle_tx_0 = signed_tx_bytes(&searcher, 0, Address::random(), 2);
    let bundle_tx_1 = signed_tx_bytes(&searcher, 1, Address::random(), 3);
    let block_value = U256::from(GWEI); // 1 gwei proposer payment
    let payment_tx =
        signed_tx_bytes(&PrivateKeySigner::random(), 0, proposer_fee_recipient, GWEI as u64);

    let merge_orders = vec![
        MergeOrderRef::Tx(TxOrderRef { index: 0, can_revert: false }),
        MergeOrderRef::Bundle(BundleOrderRef {
            txs: vec![1, 2],
            reverting_txs: vec![0],
            dropping_txs: vec![1],
        }),
    ];
    let base_block_hash = B256::random();
    write_msg(&mut stream, MergingMsgId::MergeableBlockV1, &MergeableBlockV1 {
        slot,
        builder_pubkey: BlsPublicKey::random(),
        block_value,
        builder_address: winning_builder_coinbase,
        proposer_fee_recipient,
        parent_beacon_block_root,
        allow_appending: true,
        merge_orders,
        execution_payload: demo_payload(
            parent_hash,
            winning_builder_coinbase,
            block_number,
            base_block_hash,
            vec![order_tx, bundle_tx_0, bundle_tx_1, payment_tx],
        ),
    })
    .await?;

    // -- 6. Select the merge base (the relay's top bid); this starts the
    //       merge session. The only message on the top-bid critical path.
    write_msg(&mut stream, MergingMsgId::ActivateBaseBlockV1, &ActivateBaseBlockV1 {
        slot,
        block_hash: base_block_hash,
    })
    .await?;

    // -- 7. Print whatever the builder streams back (pong, rejects, merged
    //       blocks) until it goes quiet.
    println!("--- listening for responses (10s idle timeout) ---");
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), read_msg(&mut stream)).await;
        let (msg_id, body) = match msg {
            Ok(Ok(decoded)) => decoded,
            Ok(Err(e)) => {
                println!("connection closed: {e}");
                return Ok(());
            }
            Err(_) => {
                println!("idle, done");
                return Ok(());
            }
        };
        print_response(msg_id, &body);
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: merging_relay_client <addr> <api-key-uuid> [slot] [block-number]\n\
         e.g.   merging_relay_client 127.0.0.1:9876 00000000-0000-0000-0000-000000000001 123 456"
    );
    std::process::exit(1)
}

fn print_response(msg_id: MergingMsgId, body: &[u8]) {
    match msg_id {
        MergingMsgId::PongV1 => {
            let pong = PongV1::from_ssz_bytes(body).expect("pong decode");
            println!("pong: nonce={}", pong.nonce);
        }
        MergingMsgId::RejectV1 => {
            let reject = RejectV1::from_ssz_bytes(body).expect("reject decode");
            println!(
                "reject: slot={} code={:?} subject={:?} msg={}",
                reject.slot,
                reject.code,
                reject.subject,
                String::from_utf8_lossy(&reject.msg)
            );
        }
        MergingMsgId::FatalV1 => {
            let fatal = FatalV1::from_ssz_bytes(body).expect("fatal decode");
            println!("fatal: code={:?} msg={}", fatal.code, String::from_utf8_lossy(&fatal.msg));
        }
        MergingMsgId::MergedBlockV1 => {
            let merged = MergedBlockV1::from_ssz_bytes(body).expect("merged block decode");
            let payload = &merged.execution_payload.payload_inner.payload_inner;
            println!(
                "merged block: slot={} response_id={} base={} block_hash={} txs={} \
                 proposer_value={} orders={} inclusions={}",
                merged.slot,
                merged.response_id,
                merged.base_block_hash,
                payload.block_hash,
                payload.transactions.len(),
                merged.proposer_value,
                merged.included_order_ids.len(),
                merged.builder_inclusions.len(),
            );
            for inclusion in &merged.builder_inclusions {
                println!(
                    "  builder {} revenue={} txs={}",
                    inclusion.origin_coinbase,
                    inclusion.revenue,
                    inclusion.txs.len()
                );
            }
        }
        other => println!("unexpected msg id {other:?} ({} bytes)", body.len()),
    }
}

/// EIP-2718 encoded, validly-signed EIP-1559 transfer from a throwaway key.
fn signed_tx_bytes(signer: &PrivateKeySigner, nonce: u64, to: Address, value_wei: u64) -> Vec<u8> {
    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: GWEI,
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(to),
        value: U256::from(value_wei),
        access_list: Default::default(),
        input: Default::default(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).expect("sign tx");
    TxEnvelope::Eip1559(tx.into_signed(signature)).encoded_2718()
}

fn demo_payload(
    parent_hash: B256,
    fee_recipient: Address,
    block_number: u64,
    block_hash: B256,
    txs: Vec<Vec<u8>>,
) -> ExecutionPayloadV3 {
    ExecutionPayloadV3 {
        payload_inner: ExecutionPayloadV2 {
            payload_inner: ExecutionPayloadV1 {
                parent_hash,
                fee_recipient,
                state_root: B256::random(),
                receipts_root: B256::random(),
                logs_bloom: Bloom::default(),
                prev_randao: B256::random(),
                block_number,
                gas_limit: 36_000_000,
                gas_used: 84_000,
                timestamp: now_nanos() / 1_000_000_000,
                extra_data: Bytes::from_static(b"example relay"),
                base_fee_per_gas: U256::from(GWEI),
                block_hash,
                transactions: txs.into_iter().map(Into::into).collect(),
            },
            withdrawals: vec![],
        },
        blob_gas_used: 0,
        excess_blob_gas: 0,
    }
}

fn now_nanos() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock before epoch").as_nanos() as u64
}

/// Frame: `[u32 LE payload-len][u64 LE send-ts][1B msg_type][1B flags][body]`.
async fn write_msg(
    stream: &mut TcpStream,
    msg_id: MergingMsgId,
    msg: &impl Encode,
) -> std::io::Result<()> {
    let body = msg.as_ssz_bytes();
    let mut frame = Vec::with_capacity(FRAME_HEADER_SIZE + 2 + body.len());
    frame.extend_from_slice(&((2 + body.len()) as u32).to_le_bytes());
    frame.extend_from_slice(&now_nanos().to_le_bytes());
    MergingFrameHeader::new(msg_id).append_encoded(&mut frame);
    frame.extend_from_slice(&body);
    stream.write_all(&frame).await
}

async fn read_msg(stream: &mut TcpStream) -> std::io::Result<(MergingMsgId, Vec<u8>)> {
    let mut header = [0u8; FRAME_HEADER_SIZE];
    stream.read_exact(&mut header).await?;
    let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    let frame_header = MergingFrameHeader::decode(&payload)
        .map_err(|e| std::io::Error::other(format!("bad frame header: {e}")))?;
    assert!(!frame_header.is_zstd_compressed(), "zstd was not negotiated");
    Ok((frame_header.msg_id, payload.split_off(2)))
}
