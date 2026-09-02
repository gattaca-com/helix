//! Synthetic bid-submission load for a local relay over the TCP protocol.
//!
//! `builders` connections each register with their own random BLS key and, on
//! every `interval_ms` tick, send `burst` submissions back to back — the shape
//! production sees when many builders answer the same relay event. Payloads
//! are built from live beacon payload attributes so they pass validation on a
//! relay running `is_local_dev`; `txs`/`tx_bytes`/`blobs` set their size.
use std::{
    net::SocketAddr,
    sync::Arc,
    task::Poll,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, FixedBytes, U256};
use clap::Parser;
use flux_network::tcp::{PollEvent, SendBehavior, TcpConnector};
use helix_common::{beacon::types::PayloadAttributesEvent, http::client::HttpClient};
use helix_tcp_types::{
    BidSubmissionFlags, BidSubmissionHeader, BidSubmissionResponse, MergeType, RegistrationMsg,
};
use helix_types::{
    BidTrace, BlobsBundle, BlsPublicKeyBytes, BlsSecretKey, ChainSpec, ExecutionPayload,
    ExecutionRequests, SignedBidSubmission, SignedRoot, TestRandom, Transaction, Transactions,
};
use rand::{RngCore, SeedableRng, rngs::SmallRng};
use ssz::{Decode, Encode};
use url::Url;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:4041")]
    relay: SocketAddr,
    #[arg(long, default_value = "http://127.0.0.1:4040")]
    relay_http: Url,
    #[arg(long, default_value = "http://100.74.138.87:5051")]
    beacon: Url,
    #[arg(long, default_value = "f787dfcd-979e-4d1f-b1cd-b0db8abe04c0")]
    api_key: uuid::Uuid,
    /// Concurrent synthetic builders, one TCP connection each.
    #[arg(long, default_value_t = 8)]
    builders: usize,
    /// Submissions per builder per tick, sent back to back.
    #[arg(long, default_value_t = 2)]
    burst: usize,
    #[arg(long, default_value_t = 250)]
    interval_ms: u64,
    #[arg(long, default_value_t = 200)]
    txs: usize,
    #[arg(long, default_value_t = 500)]
    tx_bytes: usize,
    #[arg(long, default_value_t = 6)]
    blobs: usize,
    #[arg(long, default_value_t = 300)]
    duration_s: u64,
    /// POST over HTTP as an untrusted builder (full signature verification on
    /// the relay) instead of the registered TCP lane.
    #[arg(long)]
    http: bool,
}

struct Builder {
    conn: TcpConnector,
    token: flux_network::Token,
    pubkey: BlsPublicKeyBytes,
    key: BlsSecretKey,
    domain: B256,
    seq: u32,
    sent: u64,
}

/// Slot context the relay validates against: the next slot's payload
/// attributes and the proposer pubkey it fabricated for that slot.
struct SlotCtx {
    slot: u64,
    parent_hash: B256,
    timestamp: u64,
    prev_randao: B256,
    withdrawals: helix_types::Withdrawals,
    proposer: BlsPublicKeyBytes,
    fee_recipient: Address,
}

fn proposer_for(relay_http: &Url, slot: u64) -> Option<BlsPublicKeyBytes> {
    let url = relay_http.join("/relay/v1/builder/validators").ok()?;
    let body: serde_json::Value = reqwest::blocking::get(url).ok()?.json().ok()?;
    body.as_array()?.iter().find_map(|e| {
        (e["slot"].as_str()?.parse::<u64>().ok()? == slot)
            .then(|| e["entry"]["message"]["pubkey"].as_str()?.parse().ok())
            .flatten()
    })
}

fn slot_ctx(ev: PayloadAttributesEvent, relay_http: &Url) -> Option<SlotCtx> {
    let d = ev.data;
    let slot = d.proposal_slot.as_u64();
    Some(SlotCtx {
        slot,
        parent_hash: d.parent_block_hash,
        timestamp: d.payload_attributes.timestamp,
        prev_randao: d.payload_attributes.prev_randao,
        withdrawals: d.payload_attributes.withdrawals,
        proposer: proposer_for(relay_http, slot)?,
        fee_recipient: d.payload_attributes.suggested_fee_recipient.parse().unwrap_or_default(),
    })
}

/// One block template per slot; submissions only differ in block hash, value
/// and builder, which is all the auction path looks at.
struct Template {
    payload: ExecutionPayload,
    blobs: Arc<BlobsBundle>,
    requests: Arc<ExecutionRequests>,
    gas_limit: u64,
    gas_used: u64,
}

fn template(ctx: &SlotCtx, args: &Args, rng: &mut SmallRng) -> Template {
    let mut payload = ExecutionPayload::random_for_test(rng);
    payload.parent_hash = ctx.parent_hash;
    payload.prev_randao = ctx.prev_randao;
    payload.timestamp = ctx.timestamp;
    payload.fee_recipient = ctx.fee_recipient;
    payload.withdrawals = ctx.withdrawals.clone();
    payload.gas_limit = 60_000_000;
    payload.gas_used = 30_000_000;
    let mut tx = vec![0u8; args.tx_bytes];
    rng.fill_bytes(&mut tx);
    let tx = Transaction::from_ssz_bytes(&tx).expect("tx bytes");
    payload.transactions = Transactions::new(vec![tx; args.txs]).expect("tx count");

    let mut blobs = BlobsBundle::with_capacity(args.blobs);
    let blob = Arc::new(alloy_consensus::Blob::random());
    for _ in 0..args.blobs {
        blobs.commitments.push(FixedBytes::<48>::ZERO).unwrap();
        blobs.proofs.extend(std::iter::repeat_n(FixedBytes::<48>::ZERO, 128));
        blobs.blobs.push(blob.clone());
    }
    Template {
        gas_limit: payload.gas_limit,
        gas_used: payload.gas_used,
        payload,
        blobs: Arc::new(blobs),
        requests: Arc::new(ExecutionRequests::default()),
    }
}

fn submission(t: &Template, ctx: &SlotCtx, builder: &Builder, value: u128) -> Vec<u8> {
    let block_hash = B256::random();
    let mut payload = t.payload.clone();
    payload.block_hash = block_hash;
    let mut sub = SignedBidSubmission {
        message: BidTrace {
            slot: ctx.slot,
            parent_hash: ctx.parent_hash,
            block_hash,
            builder_pubkey: builder.pubkey,
            proposer_pubkey: ctx.proposer,
            proposer_fee_recipient: ctx.fee_recipient,
            gas_limit: t.gas_limit,
            gas_used: t.gas_used,
            value: U256::from(value),
        },
        execution_payload: Arc::new(payload),
        blobs_bundle: t.blobs.clone(),
        execution_requests: t.requests.clone(),
        signature: Default::default(),
    };
    sub.signature = builder.key.sign(sub.message.signing_root(builder.domain)).serialize().into();
    let mut buf = Vec::with_capacity(sub.ssz_bytes_len() + 6);
    BidSubmissionHeader {
        sequence_number: builder.seq,
        merge_type: MergeType::None,
        flags: BidSubmissionFlags::empty(),
    }
    .append_encoded(&mut buf);
    sub.ssz_append(&mut buf);
    buf
}

fn main() {
    helix_common::utils::install_default_crypto_provider();
    let args = Arc::new(Args::parse());
    let mut rng = SmallRng::seed_from_u64(0x5eed);

    // Slot context is produced by the main thread from the beacon SSE stream and
    // handed to the senders; every sender fires at the barrier so a burst hits
    // the relay within microseconds, like builders answering one relay event.
    let ctx: Arc<std::sync::RwLock<Option<Arc<(SlotCtx, Template)>>>> = Default::default();
    let barrier = Arc::new(std::sync::Barrier::new(args.builders + 1));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let ok = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let err = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let last_err = Arc::new(std::sync::Mutex::new(String::new()));

    let mut threads = Vec::new();
    for i in 0..args.builders {
        let (args, ctx, barrier, stop, sent, ok, err, last_err) = (
            args.clone(),
            ctx.clone(),
            barrier.clone(),
            stop.clone(),
            sent.clone(),
            ok.clone(),
            err.clone(),
            last_err.clone(),
        );
        threads.push(std::thread::spawn(move || {
            let key = BlsSecretKey::random();
            let pubkey = BlsPublicKeyBytes::from(key.public_key().serialize());
            let reg = RegistrationMsg {
                api_key: args.api_key.into_bytes(),
                builder_pubkey: pubkey.into(),
            };
            let mut conn = TcpConnector::default()
                .with_socket_buf_size(8 * 1024 * 1024)
                .with_on_connect_msg(reg.as_ssz_bytes());
            let token = if args.http {
                flux_network::Token(0)
            } else {
                conn.connect(args.relay).expect("connect to relay")
            };
            let http = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            let url = args.relay_http.join("/relay/v1/builder/blocks").unwrap();
            let domain = ChainSpec::mainnet().get_builder_application_domain();
            let mut b = Builder { conn, token, pubkey, key, domain, seq: 0, sent: 0 };
            let mut value: u128 = 1_000_000_000_000_000 + i as u128;
            loop {
                // Frames are built before the barrier so the release costs only the write.
                let frames: Vec<Vec<u8>> =
                    ctx.read().unwrap().as_ref().map_or_else(Vec::new, |c| {
                        (0..args.burst)
                            .map(|_| {
                                value += 1_000_000_000 * args.builders as u128;
                                b.seq += 1;
                                submission(&c.1, &c.0, &b, value)
                            })
                            .collect()
                    });
                barrier.wait();
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                for frame in &frames {
                    if args.http {
                        // Body is the SSZ submission without the TCP header prefix.
                        let res = http
                            .post(url.clone())
                            .header("content-type", "application/octet-stream")
                            .body(frame[6..].to_vec())
                            .send();
                        match res {
                            Ok(r) if r.status().is_success() => {
                                ok.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            Ok(r) => {
                                *last_err.lock().unwrap() =
                                    r.text().unwrap_or_default().chars().take(80).collect();
                                err.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            Err(e) => {
                                *last_err.lock().unwrap() =
                                    e.to_string().chars().take(80).collect();
                                err.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    } else {
                        b.conn.write_or_enqueue_with(SendBehavior::Single(b.token), |buf| {
                            buf.extend_from_slice(frame)
                        });
                    }
                    b.sent += 1;
                }
                sent.fetch_add(frames.len() as u64, std::sync::atomic::Ordering::Relaxed);
                let deadline = Instant::now() + Duration::from_millis(args.interval_ms);
                while Instant::now() < deadline {
                    b.conn.poll_with(|event| {
                        if let PollEvent::Message { payload, .. } = event &&
                            let Ok(resp) = BidSubmissionResponse::from_ssz_bytes(payload)
                        {
                            if resp.error_msg.is_empty() {
                                ok.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            } else {
                                err.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                *last_err.lock().unwrap() = String::from_utf8_lossy(
                                    &resp.error_msg[..resp.error_msg.len().min(80)],
                                )
                                .into_owned();
                            }
                        }
                    });
                    std::hint::spin_loop();
                }
            }
        }));
    }

    let http = HttpClient::new().expect("http client");
    let mut sse_url = args.beacon.join("/eth/v1/events").unwrap();
    sse_url.set_query(Some("topics=payload_attributes"));
    let mut sse = http.sse_stream(sse_url);

    let start = Instant::now();
    let mut next_tick = start + Duration::from_millis(args.interval_ms);
    let mut last_report = start;
    let mut last_sent = 0;
    while start.elapsed() < Duration::from_secs(args.duration_s) {
        if let Poll::Ready(ev) = sse.poll() {
            match serde_json::from_str::<PayloadAttributesEvent>(&ev.data) {
                Ok(ev) => {
                    let slot = ev.data.proposal_slot.as_u64();
                    if ctx.read().unwrap().as_ref().is_none_or(|c| c.0.slot != slot) {
                        if let Some(c) = slot_ctx(ev, &args.relay_http) {
                            let t = template(&c, &args, &mut rng);
                            eprintln!(
                                "slot {slot}: template {} bytes",
                                t.payload.ssz_bytes_len() + 131_072 * args.blobs
                            );
                            *ctx.write().unwrap() = Some(Arc::new((c, t)));
                        }
                    }
                }
                Err(e) => eprintln!("payload_attributes parse: {e}"),
            }
        }
        let now = Instant::now();
        if now >= next_tick {
            next_tick = now + Duration::from_millis(args.interval_ms);
            barrier.wait();
        }
        if now.duration_since(last_report) >= Duration::from_secs(10) {
            last_report = now;
            let s = sent.load(std::sync::atomic::Ordering::Relaxed);
            eprintln!(
                "{:>5}s sent/10s={} total ok={} err={} {}",
                start.elapsed().as_secs(),
                s - last_sent,
                ok.load(std::sync::atomic::Ordering::Relaxed),
                err.load(std::sync::atomic::Ordering::Relaxed),
                last_err.lock().unwrap()
            );
            last_sent = s;
        }
        std::hint::spin_loop();
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    barrier.wait();
    for t in threads {
        let _ = t.join();
    }
    eprintln!(
        "done: sent={} ok={} err={}",
        sent.load(std::sync::atomic::Ordering::Relaxed),
        ok.load(std::sync::atomic::Ordering::Relaxed),
        err.load(std::sync::atomic::Ordering::Relaxed)
    );
}
