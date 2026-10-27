use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, B256, U256, address, keccak256};
use alloy_rpc_types::beacon::BlsPublicKey;
use alloy_signer_local::PrivateKeySigner;
use crossbeam_channel::{Receiver, Sender};
use ethrex_blockchain::{
    Blockchain, BlockchainOptions, BlockchainType,
    payload::{BuildPayloadArgs, create_payload},
};
use ethrex_common::types::{ELASTICITY_MULTIPLIER, Genesis, GenesisAccount};
use ethrex_storage::Store;
use helix_tcp_types::merging::{
    builder_to_relay::{MergedBlockV1, RejectCode},
    control::{BuilderCollateral, RelayConfigV1},
    order::{BundleOrderRef, MergeOrderRef, TxOrderRef, bundle_order_hash, order_id},
    relay_to_builder::{MergeableBlockV1, SlotStartV1},
};
use ssz::Encode;

use crate::{
    convert::{b256, block_to_payload_v3, eaddr, payload_v3_to_block},
    merge::{
        EngineConfig, EngineEvent, EngineOutput, MergeEngine,
        ingest::Pool,
        payment::{SafePayment, payment_tx},
    },
    testing::{
        ETH, GWEI, deploy_payment_forwarder, dev_genesis_store_with, forwarder_calldata,
        funded_signers, signed_call, signed_transfer,
    },
};

const SLOT: u64 = 1;
const SAFE: Address = address!("5637a7552003411c8953Feba6E38951a32Ab9c49");
const SAFE_SINGLETON: Address = address!("41675c099f32341bf84bfc5382af534df5c7461a");
const MULTISEND: Address = address!("40A2aCCbd92BCA938b02010E17A5b8929b49130D");

struct Fixture {
    store: Store,
    blockchain: Arc<Blockchain>,
    genesis_hash: ethrex_common::H256,
    genesis_timestamp: u64,
    chain_id: u64,
    gas_limit: u64,
    signers: Vec<PrivateKeySigner>,
    proposer: Address,
    block_value: U256,
    relay_config: RelayConfigV1,
}

impl Fixture {
    async fn new(edit: impl FnOnce(&mut Genesis)) -> Self {
        let (store, genesis) = dev_genesis_store_with(edit).await;
        let block = genesis.get_block();
        let blockchain = Blockchain::new(store.clone(), BlockchainOptions {
            r#type: BlockchainType::L1,
            ..Default::default()
        });
        let signers = funded_signers(&genesis, 8);
        let relay_config = RelayConfigV1 {
            relay_fee_recipient: signers[4].address(),
            multisend_contract: Address::repeat_byte(0x88),
            relay_bps: 2500,
            merged_builder_bps: 2500,
            winning_builder_bps: 2500,
            distribution_gas_limit: 140_000,
            builder_collaterals: vec![BuilderCollateral {
                builder_coinbase: signers[0].address(),
                collateral_safe: signers[5].address(),
            }],
        };
        Self {
            store,
            blockchain: Arc::new(blockchain),
            genesis_hash: block.hash(),
            genesis_timestamp: block.header.timestamp,
            chain_id: genesis.config.chain_id,
            gas_limit: block.header.gas_limit,
            signers,
            proposer: Address::repeat_byte(0x77),
            block_value: U256::from(ETH / 2),
            relay_config,
        }
    }

    /// An engine on a started slot.
    fn start(&self) -> (Sender<EngineEvent>, Receiver<EngineOutput>) {
        let config = EngineConfig {
            relay_signer: self.signers[6].clone(),
            max_blocks_per_slot: 64,
            max_orders_per_slot: 1024,
            max_builders: 8,
            min_value_increase_wei: U256::ZERO,
            max_base_age: Duration::from_secs(5),
            warmup: false,
            core: None,
            disallow: Default::default(),
            record: None,
        };
        let (event_tx, event_rx) = crossbeam_channel::bounded(1024);
        let (output_tx, output_rx) = crossbeam_channel::bounded(1024);
        MergeEngine::spawn(config, self.store.clone(), event_rx, output_tx);
        let slot_start = SlotStartV1 {
            slot: SLOT,
            parent_hash: b256(self.genesis_hash),
            proposer_fee_recipient: self.proposer,
            parent_beacon_block_root: B256::ZERO,
            apply_blacklist: false,
        };
        event_tx.send(EngineEvent::RelayConfig(self.relay_config.clone())).unwrap();
        event_tx.send(EngineEvent::SlotStart(slot_start)).unwrap();
        (event_tx, output_rx)
    }

    fn transfer(&self, from: usize, to: Address, value: U256) -> Vec<u8> {
        signed_transfer(&self.signers[from], self.chain_id, 0, to, value, 100 * GWEI, GWEI)
    }

    fn tx(
        &self,
        from: usize,
        nonce: u64,
        to: Address,
        gas_limit: u64,
        value: U256,
        input: Vec<u8>,
    ) -> Vec<u8> {
        use alloy_consensus::SignableTransaction;
        use alloy_signer::SignerSync;
        let tx = alloy_consensus::TxEip1559 {
            chain_id: self.chain_id,
            nonce,
            gas_limit,
            max_fee_per_gas: 100 * GWEI,
            max_priority_fee_per_gas: GWEI,
            to: to.into(),
            value,
            access_list: Default::default(),
            input: input.into(),
        };
        let signature = self.signers[from].sign_hash_sync(&tx.signature_hash()).unwrap();
        alloy_eips::eip2718::Encodable2718::encoded_2718(&alloy_consensus::TxEnvelope::from(
            tx.into_signed(signature),
        ))
    }

    /// A base from builder 0: a user transfer, then a direct proposer payment.
    fn base(&self) -> MergeableBlockV1 {
        let user = self.transfer(1, Address::repeat_byte(0x55), U256::from(ETH));
        let payment = signed_transfer(
            &self.signers[0],
            self.chain_id,
            0,
            self.proposer,
            self.block_value,
            100 * GWEI,
            0,
        );
        self.base_from_txs(vec![user, payment])
    }

    fn base_from_txs(&self, txs: Vec<Vec<u8>>) -> MergeableBlockV1 {
        let builder = &self.signers[0];
        let args = BuildPayloadArgs {
            parent: self.genesis_hash,
            timestamp: self.genesis_timestamp + 12,
            fee_recipient: eaddr(builder.address()),
            random: ethrex_common::H256::zero(),
            withdrawals: Some(Vec::new()),
            beacon_root: Some(ethrex_common::H256::zero()),
            slot_number: None,
            version: 3,
            elasticity_multiplier: ELASTICITY_MULTIPLIER,
            gas_ceil: self.gas_limit,
        };
        let template = create_payload(&args, &self.store, Default::default()).unwrap();
        let txs = txs
            .iter()
            .map(|bytes| ethrex_common::types::Transaction::decode_canonical(bytes).unwrap())
            .collect();
        let built = self.blockchain.build_payload_with_transactions(template, txs).unwrap();
        MergeableBlockV1 {
            slot: SLOT,
            builder_pubkey: Default::default(),
            block_value: self.block_value,
            builder_address: builder.address(),
            proposer_fee_recipient: self.proposer,
            parent_beacon_block_root: B256::ZERO,
            allow_appending: true,
            merge_orders: Vec::new(),
            execution_payload: block_to_payload_v3(&built.payload),
        }
    }

    /// A non-appendable block from builder 2 carrying `txs` as `orders`.
    fn orders(
        &self,
        base: &MergeableBlockV1,
        pubkey: u8,
        txs: Vec<Vec<u8>>,
        orders: Vec<MergeOrderRef>,
    ) -> MergeableBlockV1 {
        let mut payload = base.execution_payload.clone();
        let v1 = &mut payload.payload_inner.payload_inner;
        v1.block_hash = keccak256(txs.concat());
        v1.transactions = txs.into_iter().map(Into::into).collect();
        v1.fee_recipient = self.signers[2].address();
        MergeableBlockV1 {
            slot: SLOT,
            builder_pubkey: BlsPublicKey::repeat_byte(pubkey),
            block_value: U256::from(ETH / 10),
            builder_address: self.signers[2].address(),
            proposer_fee_recipient: self.proposer,
            parent_beacon_block_root: B256::ZERO,
            allow_appending: false,
            merge_orders: orders,
            execution_payload: payload,
        }
    }

    /// Imports `merged` as the chain would: consensus validity, not just our own roots.
    fn assert_valid(&self, merged: &MergedBlockV1) {
        let payload = &merged.execution_payload;
        let block = payload_v3_to_block(payload, B256::ZERO, &merged.execution_requests, None);
        self.blockchain.add_block(block.unwrap()).expect("merged block must import");
    }
}

fn tx_order(index: u16) -> MergeOrderRef {
    MergeOrderRef::Tx(TxOrderRef { index, can_revert: false })
}

fn bundle(count: u16, reverting_txs: Vec<u16>, dropping_txs: Vec<u16>) -> Vec<MergeOrderRef> {
    let txs = (0..count).collect();
    vec![MergeOrderRef::Bundle(BundleOrderRef {
        txs,
        reverting_txs,
        dropping_txs,
        latest_only: true,
    })]
}

fn send(events: &Sender<EngineEvent>, msg: &MergeableBlockV1) {
    let body = msg.as_ssz_bytes();
    let recv_ns = crate::utils::utcnow_ns();
    events.send(EngineEvent::MergeableBlock { body, recv_ns, generation: 0 }).unwrap();
}

/// The highest-value block merged before the engine goes quiet.
fn best(out: &Receiver<EngineOutput>) -> Box<MergedBlockV1> {
    let mut best: Option<Box<MergedBlockV1>> = None;
    while let Ok(output) = out.recv_timeout(Duration::from_secs(3)) {
        let msg = match output {
            EngineOutput::Merged { msg, .. } => msg,
            EngineOutput::Reject { msg, .. } => {
                panic!("rejected: {:?} {}", msg.code, String::from_utf8_lossy(&msg.msg))
            }
        };
        if best.as_ref().is_none_or(|b| msg.proposer_value > b.proposer_value) {
            best = Some(msg);
        }
    }
    best.expect("nothing merged")
}

#[tokio::test(flavor = "multi_thread")]
async fn merges_an_order_into_a_valid_block() {
    let fixture = Fixture::new(|_| {}).await;
    let base = fixture.base();
    let value = U256::from(ETH / 5);
    let tx = fixture.transfer(3, fixture.signers[0].address(), value);
    let order = fixture.orders(&base, 1, vec![tx], vec![tx_order(0)]);
    let (events, out) = fixture.start();
    send(&events, &base);
    send(&events, &order);

    let merged = best(&out);
    let base_hash = base.execution_payload.payload_inner.payload_inner.block_hash;
    assert_eq!(merged.base_block_hash, base_hash);
    let v1 = &merged.execution_payload.payload_inner.payload_inner;
    assert_eq!(v1.transactions.len(), 4, "base txs, the order, the distribution");
    assert_eq!(merged.included_order_ids.len(), 1);
    let inclusion = &merged.builder_inclusions[0];
    assert_eq!(inclusion.origin_coinbase, fixture.signers[2].address());
    assert!(inclusion.contribution >= value);
    let added = merged.proposer_value - fixture.block_value;
    assert!(added > U256::ZERO);
    assert!(added <= inclusion.contribution / U256::from(4) + U256::from(1));
    fixture.assert_valid(&merged);
}

/// Builders pay the proposer through a forwarder contract, so only the proposer's balance delta
/// across the last tx proves the bid.
#[tokio::test(flavor = "multi_thread")]
async fn a_base_paying_through_a_contract_is_merged() {
    let fixture = Fixture::new(deploy_payment_forwarder).await;
    let user = fixture.transfer(1, Address::repeat_byte(0x55), U256::from(ETH));
    let payment = signed_call(
        &fixture.signers[0],
        fixture.chain_id,
        0,
        helix_common::PAYMENT_FORWARDER,
        fixture.block_value,
        forwarder_calldata(fixture.genesis_timestamp + 12, fixture.proposer),
    );
    let base = fixture.base_from_txs(vec![user, payment]);
    let tx = fixture.transfer(3, fixture.signers[0].address(), U256::from(ETH / 5));
    let order = fixture.orders(&base, 1, vec![tx], vec![tx_order(0)]);
    let (events, out) = fixture.start();
    send(&events, &base);
    send(&events, &order);

    let merged = best(&out);
    assert!(merged.proposer_value > fixture.block_value);
    fixture.assert_valid(&merged);
}

/// The mainnet Safe v1.4.1 collateral setup, owned by the relay signer (signer 6).
fn deploy_collateral_safe(genesis: &mut Genesis) {
    let owner = funded_signers(genesis, 8)[6].address();
    let word = |value: U256| ethrex_common::U256::from_big_endian(&value.to_be_bytes::<32>());
    let owner_slot = |key: Address| {
        let mut preimage = B256::left_padding_from(key.as_slice()).to_vec();
        preimage.extend_from_slice(B256::from(U256::from(2)).as_slice());
        word(keccak256(preimage).into())
    };
    let sentinel = Address::with_last_byte(1);
    let account = |code: &str, storage, balance| GenesisAccount {
        code: hex::decode(code.trim()).unwrap().into(),
        storage,
        balance,
        nonce: 1,
    };
    let storage = [
        (word(U256::ZERO), word(U256::from_be_slice(SAFE_SINGLETON.as_slice()))),
        (word(U256::from(3)), word(U256::from(1))),
        (word(U256::from(4)), word(U256::from(1))),
        (owner_slot(sentinel), word(U256::from_be_slice(owner.as_slice()))),
        (owner_slot(owner), word(U256::from_be_slice(sentinel.as_slice()))),
    ]
    .into_iter()
    .collect();
    let balance = word(U256::from(100 * ETH));
    let safe = account(include_str!("testdata/safe_proxy.hex"), storage, balance);
    let singleton =
        account(include_str!("testdata/safe_singleton.hex"), Default::default(), 0.into());
    let multisend =
        account(include_str!("testdata/multisend_call_only.hex"), Default::default(), 0.into());
    genesis.alloc.insert(eaddr(SAFE), safe);
    genesis.alloc.insert(eaddr(SAFE_SINGLETON), singleton);
    genesis.alloc.insert(eaddr(MULTISEND), multisend);
}

/// A base that pays the proposer through the collateral Safe moves the Safe's nonce, so the
/// distribution must be signed over the post-base nonce.
#[tokio::test(flavor = "multi_thread")]
async fn distributes_through_the_collateral_safe_after_a_safe_paid_base() {
    let mut fixture = Fixture::new(deploy_collateral_safe).await;
    fixture.relay_config.multisend_contract = MULTISEND;
    fixture.relay_config.builder_collaterals[0].collateral_safe = SAFE;

    // The builder relays a Safe call the owner signed; only the Safe signature is checked.
    let p = SafePayment {
        safe: SAFE,
        safe_balance: U256::from(100 * ETH),
        safe_nonce: 0,
        signer_nonce: 0,
        chain_id: fixture.chain_id,
        gas_limit: 300_000,
        base_fee: 100 * GWEI,
        multisend: MULTISEND,
    };
    let signed = payment_tx(&fixture.signers[6], &p, &[(fixture.proposer, fixture.block_value)]);
    let signed = ethrex_common::types::Transaction::decode_canonical(&signed.unwrap()).unwrap();
    let payment = fixture.tx(0, 0, SAFE, 300_000, U256::ZERO, signed.data().to_vec());
    let user = fixture.transfer(1, Address::repeat_byte(0x55), U256::from(ETH));
    let base = fixture.base_from_txs(vec![user, payment]);
    let tx = fixture.transfer(3, fixture.signers[0].address(), U256::from(ETH / 5));
    let order = fixture.orders(&base, 1, vec![tx], vec![tx_order(0)]);
    let (events, out) = fixture.start();
    send(&events, &order);
    send(&events, &base);

    let merged = best(&out);
    assert_eq!(merged.included_order_ids.len(), 1);
    fixture.assert_valid(&merged);
}

/// A droppable tx whose gas limit no longer fits once earlier orders fill the block must be
/// dropped: consensus checks the limit, not the gas the tx uses.
#[tokio::test(flavor = "multi_thread")]
async fn drops_a_droppable_tx_whose_gas_limit_no_longer_fits() {
    let burner = Address::repeat_byte(0x66);
    let fixture = Fixture::new(|genesis| {
        // JUMPDEST PUSH1 0 JUMP: burns all gas.
        genesis.alloc.insert(eaddr(burner), GenesisAccount {
            code: hex::decode("5b600056").unwrap().into(),
            storage: Default::default(),
            balance: 0.into(),
            nonce: 1,
        });
    })
    .await;
    let base = fixture.base();
    let coinbase = fixture.signers[0].address();
    let burn = |nonce| fixture.tx(3, nonce, burner, 12_300_000, U256::ZERO, Vec::new());
    let pay = fixture.tx(3, 2, coinbase, 21_000, U256::from(ETH / 5), Vec::new());
    let filler =
        fixture.orders(&base, 0xa1, vec![burn(0), burn(1), pay], bundle(3, vec![0, 1], vec![]));
    let wide = fixture.tx(5, 0, Address::repeat_byte(0x55), 400_000, U256::from(1), Vec::new());
    let wide_hash = keccak256(&wide);
    let pay = fixture.tx(4, 0, coinbase, 21_000, U256::from(ETH / 10), Vec::new());
    let late = fixture.orders(&base, 0xb1, vec![pay, wide], bundle(2, vec![], vec![1]));
    let (events, out) = fixture.start();
    for msg in [&filler, &late, &base] {
        send(&events, msg);
    }

    let merged = best(&out);
    assert_eq!(merged.included_order_ids.len(), 2);
    let txs = &merged.execution_payload.payload_inner.payload_inner.transactions;
    assert!(txs.iter().all(|tx| keccak256(tx) != wide_hash));
    fixture.assert_valid(&merged);
}

/// A latest-only bundle missing from its builder's newer submission is withdrawn for the slot.
#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawn_latest_only_bundle_is_not_merged() {
    let fixture = Fixture::new(|_| {}).await;
    let base = fixture.base();
    let coinbase = fixture.signers[0].address();
    let first = fixture.transfer(3, coinbase, U256::from(ETH / 5));
    let second = fixture.transfer(4, coinbase, U256::from(ETH / 5));
    let kept = bundle_order_hash(&[keccak256(&second)]);
    let older = fixture.orders(&base, 1, vec![first], bundle(1, vec![], vec![]));
    let newer = fixture.orders(&base, 1, vec![second], bundle(1, vec![], vec![]));
    let (events, out) = fixture.start();
    for msg in [&older, &newer, &base] {
        send(&events, msg);
    }

    let merged = best(&out);
    assert_eq!(merged.included_order_ids, vec![order_id(kept, &BlsPublicKey::repeat_byte(1))]);
    fixture.assert_valid(&merged);
}

/// A block rejected for an unresolved reference or a wrong parent still caches its own txs, or
/// every later block that references them fails too.
#[tokio::test]
async fn a_rejected_block_still_caches_its_own_txs() {
    let fixture = Fixture::new(|_| {}).await;
    let base = fixture.base();
    let own = fixture.transfer(3, Address::repeat_byte(0x55), U256::from(1));
    let parent = b256(fixture.genesis_hash);
    let mut pool = Pool::default();

    let unknown = B256::repeat_byte(0xab).to_vec();
    let failing = fixture.orders(&base, 1, vec![own.clone(), unknown], vec![tx_order(0)]);
    let reject = pool.block(&failing.as_ssz_bytes(), 0, SLOT, parent, 64, 64).reject;
    let reject = reject.expect("an unresolved reference rejects the block");
    assert_eq!(reject.code, RejectCode::InvalidOrder);
    assert!(reject.msg.starts_with("unresolved tx hash reference"), "{}", reject.msg);

    let by_ref = keccak256(&own).to_vec();
    let referencing = fixture.orders(&base, 2, vec![by_ref], vec![tx_order(0)]);
    let ingested = pool.block(&referencing.as_ssz_bytes(), 0, SLOT, parent, 64, 64);
    assert!(ingested.reject.is_none());
    assert_eq!(pool.orders.len(), 1);

    let other = fixture.transfer(4, Address::repeat_byte(0x55), U256::from(1));
    let wrong_parent = fixture.orders(&base, 3, vec![other.clone()], vec![tx_order(0)]);
    let reject = pool.block(&wrong_parent.as_ssz_bytes(), 0, SLOT, B256::ZERO, 64, 64).reject;
    assert_eq!(reject.expect("a wrong parent rejects the block").code, RejectCode::HeadMismatch);

    let by_ref = keccak256(&other).to_vec();
    let referencing = fixture.orders(&base, 4, vec![by_ref], vec![tx_order(0)]);
    let ingested = pool.block(&referencing.as_ssz_bytes(), 0, SLOT, parent, 64, 64);
    assert!(ingested.reject.is_none());
    assert_eq!(pool.orders.len(), 2);
}

/// Replays a slot `record` wrote, on its recorded timing, against a store holding the state it
/// read; reports first-emission latency per base.
///
/// `REPLAY_DIR=<record dir>/<slot> cargo test --release -p helix-builder replay_recording --
/// --ignored --nocapture`. `REPLAY_VALIDATE=<n>` imports every nth merged block with ethrex.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn replay_recording() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_default())
        .try_init();
    ethrex_crypto::kzg::warm_up_trusted_setup();
    let dir = std::path::PathBuf::from(std::env::var("REPLAY_DIR").expect("REPLAY_DIR"));
    let (outputs, validator, beacon_root) = replay::slot(&dir).await;

    let mut first: rustc_hash::FxHashMap<B256, (u64, u64)> = Default::default();
    let mut rejects: std::collections::BTreeMap<String, usize> = Default::default();
    let mut merged = 0;
    for output in &outputs {
        match output {
            EngineOutput::Merged { msg, .. } => {
                merged += 1;
                let t = msg.trace;
                first.entry(msg.base_block_hash).or_insert((t.base_block_recv_ns, t.finalize_ns));
            }
            EngineOutput::Reject { msg, .. } => {
                *rejects.entry(format!("{:?}", msg.code)).or_default() += 1;
            }
        }
    }
    let mut ms: Vec<f64> =
        first.values().map(|(recv, done)| done.saturating_sub(*recv) as f64 / 1e6).collect();
    ms.sort_unstable_by(f64::total_cmp);
    let q =
        |p: f64| ms.get(((ms.len().max(1) - 1) as f64 * p) as usize).copied().unwrap_or(f64::NAN);
    println!("merged={merged} bases_emitted={} rejects={rejects:?}", first.len());
    println!("first emit p50 {:.2}ms p90 {:.2}ms max {:.2}ms", q(0.5), q(0.9), q(1.0));

    for family in crate::metrics::BUILDER_METRICS_REGISTRY.gather() {
        let name = family.get_name();
        for metric in family.get_metric() {
            let label = |key: &str| {
                metric.get_label().iter().find(|l| l.get_name() == key).map(|l| l.get_value())
            };
            let stage = label("stage").unwrap_or("?");
            if name.ends_with("merge_rejection_total") {
                let reason = label("reason").unwrap_or("?");
                println!("rejected {stage}/{reason} n={}", metric.get_counter().get_value());
            } else if name.ends_with("merge_stage_latency_us") {
                let h = metric.get_histogram();
                let n = h.get_sample_count();
                let mean_ms = h.get_sample_sum() / n.max(1) as f64 / 1e3;
                println!("stage {stage:<18} n={n:<6} mean {mean_ms:>8.2}ms");
            }
        }
    }

    if let Some(every) = std::env::var("REPLAY_VALIDATE").ok().and_then(|v| v.parse::<usize>().ok())
    {
        let (mut ok, mut failed) = (0, std::collections::BTreeMap::<String, usize>::new());
        let merged = outputs.iter().filter_map(|output| match output {
            EngineOutput::Merged { msg, .. } => Some(msg),
            EngineOutput::Reject { .. } => None,
        });
        for msg in merged.step_by(every.max(1)) {
            let requests = &msg.execution_requests;
            let block = payload_v3_to_block(&msg.execution_payload, beacon_root, requests, None);
            match validator.add_block(block.unwrap()) {
                Ok(()) => ok += 1,
                Err(err) => {
                    *failed.entry(err.to_string().chars().take(120).collect()).or_default() += 1
                }
            }
        }
        println!("validated ok={ok} failed={failed:?}");
    }
}

mod replay {
    use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

    use alloy_primitives::B256;
    use ethrex_blockchain::{Blockchain, BlockchainOptions, BlockchainType};
    use ethrex_common::{
        Address, H256, U256,
        types::{AccountState, BlockHeader, ChainConfig, GenesisAccount},
    };
    use ethrex_rlp::{decode::RLPDecode, encode::RLPEncode};
    use ethrex_storage::Store;
    use helix_tcp_types::merging::{
        control::RelayConfigV1,
        relay_to_builder::{MergeableBlockV1, SlotStartV1},
    };
    use ssz::{Decode, Encode};

    use crate::{
        convert::{b256, eaddr},
        merge::{
            EngineConfig, EngineEvent, EngineOutput, MergeEngine,
            record::{
                KIND_MERGEABLE, KIND_RELAY_CONFIG, KIND_SLOT_END, KIND_SLOT_START, RecordMeta,
                SAFE_SENTINEL, TrieProofs, replay_signer, safe_owner_slot,
            },
        },
        testing::ETH,
    };

    /// Runs the recorded slot through a fresh engine. Returns its outputs, a chain to import
    /// merged blocks into, and the slot's beacon root.
    pub async fn slot(dir: &Path) -> (Vec<EngineOutput>, Arc<Blockchain>, B256) {
        let read = |name: &str| std::fs::read(dir.join(name)).unwrap();
        let meta: RecordMeta = serde_json::from_slice(&read("meta.json")).unwrap();
        let chain_config: ChainConfig = serde_json::from_slice(&read("chain_config.json")).unwrap();
        let mut alloc: BTreeMap<Address, GenesisAccount> =
            serde_json::from_slice(&read("alloc.json")).unwrap();
        let block_hashes: BTreeMap<u64, H256> =
            serde_json::from_slice(&read("block_hashes.json")).unwrap();
        let events = zstd::decode_all(&read("events.bin.zst")[..]).unwrap();
        let mut frames = Vec::new();
        let mut at = 0;
        while at < events.len() {
            let kind = events[at];
            let ns = u64::from_le_bytes(events[at + 1..at + 9].try_into().unwrap());
            let len = u32::from_le_bytes(events[at + 9..at + 13].try_into().unwrap()) as usize;
            frames.push((kind, ns, events[at + 13..at + 13 + len].to_vec()));
            at += 13 + len;
        }

        // The replay key stands in for the relay's: inserted as a Safe owner on every collateral.
        let signer = replay_signer();
        let replay_owner = eaddr(signer.address());
        let word =
            |a: Address| U256::from_big_endian(B256::left_padding_from(a.as_bytes()).as_slice());
        let owner_slot = |owner: Address| U256::from_big_endian(safe_owner_slot(owner).as_bytes());
        let relay_config = frames
            .iter()
            .find(|(kind, ..)| *kind == KIND_RELAY_CONFIG)
            .map(|(_, _, body)| RelayConfigV1::from_ssz_bytes(body).unwrap())
            .expect("recording has a relay config");
        for collateral in &relay_config.builder_collaterals {
            if let Some(safe) = alloc.get_mut(&eaddr(collateral.collateral_safe)) {
                safe.storage.insert(owner_slot(SAFE_SENTINEL), word(replay_owner));
                safe.storage.insert(owner_slot(replay_owner), word(meta.relay_signer));
            }
        }
        alloc.entry(replay_owner).or_insert(GenesisAccount {
            code: Default::default(),
            storage: Default::default(),
            balance: U256::from(100 * ETH),
            nonce: 0,
        });

        let proofs = zstd::decode_all(&read("trie.json.zst")[..]).unwrap();
        let proofs: TrieProofs = serde_json::from_slice(&proofs).unwrap();
        let mut accounts: BTreeMap<Address, AccountState> =
            serde_json::from_slice(&read("accounts.json")).unwrap();
        accounts
            .entry(replay_owner)
            .or_insert(AccountState { balance: U256::from(100 * ETH), ..Default::default() });
        let parent = BlockHeader::decode(&read("parent_header.rlp")).unwrap();
        let path = std::env::temp_dir().join(format!("helix-replay-{}", meta.slot));
        let root = parent.state_root;
        let store = seeded_store(&path, &chain_config, &proofs, &accounts, &alloc, root).await;
        let parent_hash = parent.hash();
        store.add_block_header(parent_hash, parent.clone()).await.unwrap();
        store.add_block_number(parent_hash, parent.number).await.unwrap();
        let canonical = block_hashes.into_iter().filter(|(n, _)| *n != parent.number);
        let canonical = canonical.chain([(parent.number, parent_hash)]).collect();
        store.forkchoice_update(canonical, parent.number, parent_hash, None, None).await.unwrap();
        let blockchain = Arc::new(Blockchain::new(store.clone(), BlockchainOptions {
            r#type: BlockchainType::L1,
            ..Default::default()
        }));

        let max_base_age_ms =
            std::env::var("REPLAY_MAX_BASE_AGE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(50);
        let config = EngineConfig {
            relay_signer: signer,
            max_blocks_per_slot: 1 << 20,
            max_orders_per_slot: 8192,
            max_builders: 16,
            min_value_increase_wei: alloy_primitives::U256::ZERO,
            max_base_age: Duration::from_millis(max_base_age_ms),
            warmup: std::env::var_os("REPLAY_NO_WARMUP").is_none(),
            core: None,
            disallow: Default::default(),
            record: None,
        };
        let (event_tx, event_rx) = crossbeam_channel::bounded(1 << 16);
        let (output_tx, output_rx) = crossbeam_channel::bounded(1 << 16);
        let engine = MergeEngine::spawn(config, store, event_rx, output_tx);
        let collector = std::thread::spawn(move || output_rx.iter().collect::<Vec<_>>());

        let mut beacon_root = B256::ZERO;
        let recorded_start = frames[0].1;
        let start = std::time::Instant::now();
        let mut replayed = None;
        for (kind, ns, body) in frames {
            let due = Duration::from_nanos(ns.saturating_sub(recorded_start));
            if let Some(wait) = due.checked_sub(start.elapsed()) {
                std::thread::sleep(wait);
            }
            let event = match kind {
                KIND_RELAY_CONFIG => {
                    EngineEvent::RelayConfig(RelayConfigV1::from_ssz_bytes(&body).unwrap())
                }
                KIND_SLOT_START => {
                    if replayed.is_some() {
                        break;
                    }
                    // Recordings before `apply_blacklist` lack its trailing byte.
                    let mut body = body;
                    body.resize(body.len().max(93), 0);
                    let mut msg = SlotStartV1::from_ssz_bytes(&body).unwrap();
                    replayed = Some(msg.slot);
                    beacon_root = msg.parent_beacon_block_root;
                    msg.parent_hash = b256(parent_hash);
                    EngineEvent::SlotStart(msg)
                }
                KIND_MERGEABLE => {
                    let mut msg = MergeableBlockV1::from_ssz_bytes(&body).unwrap();
                    msg.execution_payload.payload_inner.payload_inner.parent_hash =
                        b256(parent_hash);
                    let recv_ns = crate::utils::utcnow_ns();
                    EngineEvent::MergeableBlock { body: msg.as_ssz_bytes(), recv_ns, generation: 0 }
                }
                KIND_SLOT_END => break,
                _ => continue,
            };
            event_tx.send(event).unwrap();
        }
        // Lets the last bases run out their age before the slot ends.
        std::thread::sleep(Duration::from_millis(600));
        if let Some(slot) = replayed {
            event_tx.send(EngineEvent::SlotEnd { slot }).unwrap();
        }
        drop(event_tx);
        engine.join().unwrap();
        (collector.join().unwrap(), blockchain, beacon_root)
    }

    /// A RocksDB store holding the recorded mainnet trie paths and the flat values the slot
    /// read, rooted at the real parent state root.
    async fn seeded_store(
        path: &Path,
        chain_config: &ChainConfig,
        proofs: &TrieProofs,
        accounts: &BTreeMap<Address, AccountState>,
        alloc: &BTreeMap<Address, GenesisAccount>,
        state_root: H256,
    ) -> Store {
        use ethrex_storage::{
            EngineType,
            api::tables::{
                ACCOUNT_FLATKEYVALUE, ACCOUNT_TRIE_NODES, MISC_VALUES, STORAGE_FLATKEYVALUE,
                STORAGE_TRIE_NODES,
            },
        };
        use ethrex_trie::{Nibbles, Node, NodeHash, NodeRef};
        let _ = std::fs::remove_dir_all(path);
        let mut store = Store::new(path, EngineType::RocksDB).unwrap();
        store.set_chain_config(chain_config).await.unwrap();
        let seed = |nodes: &[Vec<u8>], root: H256, prefix: Option<H256>, table| {
            let by_hash: rustc_hash::FxHashMap<H256, &Vec<u8>> = nodes
                .iter()
                .map(|rlp| (H256::from(ethrex_crypto::keccak::keccak_hash(rlp)), rlp))
                .collect();
            let mut stack = vec![(root, Nibbles::default())];
            while let Some((hash, at)) = stack.pop() {
                let Some(rlp) = by_hash.get(&hash) else { continue };
                let key = ethrex_storage::apply_prefix(prefix, at.clone()).into_vec();
                store.write(table, key, (*rlp).clone()).unwrap();
                let child = |child: &NodeRef| match child {
                    NodeRef::Hash(NodeHash::Hashed(hash)) => Some(*hash),
                    _ => None,
                };
                match Node::decode(rlp).unwrap() {
                    Node::Branch(branch) => {
                        for (nibble, choice) in branch.choices.iter().enumerate() {
                            let next = child(choice).map(|h| (h, at.append_new(nibble as u8)));
                            stack.extend(next);
                        }
                    }
                    Node::Extension(ext) => {
                        stack.extend(child(&ext.child).map(|h| (h, at.concat(&ext.prefix))));
                    }
                    Node::Leaf(_) => {}
                }
            }
        };
        seed(&proofs.state, state_root, None, ACCOUNT_TRIE_NODES);
        for (address, account) in accounts {
            let hashed = H256::from_slice(&ethrex_storage::hash_address(address));
            let key = Nibbles::from_bytes(hashed.as_bytes()).into_vec();
            store.write(ACCOUNT_FLATKEYVALUE, key, account.encode_to_vec()).unwrap();
            if let Some(nodes) = proofs.storage.get(&hashed) {
                seed(nodes, account.storage_root, Some(hashed), STORAGE_TRIE_NODES);
            }
        }
        for (address, genesis_account) in alloc {
            let hashed = H256::from_slice(&ethrex_storage::hash_address(address));
            for (slot, value) in &genesis_account.storage {
                if value.is_zero() {
                    continue;
                }
                let slot = H256::from(slot.to_big_endian());
                let key = Nibbles::from_bytes(&ethrex_storage::hash_key(&slot));
                let key = ethrex_storage::apply_prefix(Some(hashed), key).into_vec();
                store.write(STORAGE_FLATKEYVALUE, key, value.encode_to_vec()).unwrap();
            }
            if !genesis_account.code.is_empty() {
                let code = ethrex_common::types::Code::from_bytecode(
                    genesis_account.code.clone(),
                    &ethrex_crypto::native::NativeCrypto,
                );
                store.add_account_code(code).await.unwrap();
            }
        }
        store.write(MISC_VALUES, b"last_written".to_vec(), vec![0xff]).unwrap();
        drop(store);
        let mut store = Store::new(path, EngineType::RocksDB).unwrap();
        store.set_chain_config(chain_config).await.unwrap();
        store
    }
}
