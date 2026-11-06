use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, U256, address, keccak256};
use alloy_rpc_types::beacon::BlsPublicKey;
use alloy_signer_local::PrivateKeySigner;
use ethrex_blockchain::{
    Blockchain, BlockchainOptions, BlockchainType,
    payload::{BuildPayloadArgs, create_payload},
};
use ethrex_common::types::{ELASTICITY_MULTIPLIER, Genesis, GenesisAccount};
use ethrex_storage::Store;
use flux::{
    spine::{DCacheRead, SpineAdapter, SpineDCacheConsumer},
    tile::Tile,
};
use helix_common::payment::multisend_paid_amount;
use helix_tcp_types::merging::{
    builder_to_relay::{MergedBlockV1, MergedBlockV2, RejectCode, RejectV1},
    control::{BuilderCollateral, RelayConfigV1},
    order::{BundleOrderRef, MergeOrderRef, TxOrderRef, bundle_order_hash, order_id},
    relay_to_builder::{MergeableBlockV1, SlotStartV1},
};
use ssz::{Decode, Encode};

use crate::{
    convert::{
        Amsterdam, b256, block_to_payload_v3, decode_execution_requests, eaddr, payload_v3_to_block,
    },
    merge::{
        Builder, EngineConfig, MergeEngine, STACK_SIZE,
        block::BlockMerger,
        payment::{SafePayment, payment_tx},
        pool::{BaseBlock, Pool, Submitter},
        select_base,
    },
    spine::{BuilderSpine, EventKind, FromEngine, OutputKind, ToEngine},
    testing::{
        ETH, GWEI, TestSpine, deploy_amsterdam_predeploys, deploy_payment_forwarder,
        dev_genesis_store_with, forwarder_calldata, funded_signers, signed_call, signed_transfer,
    },
    utils::utcnow_ns,
};

const SLOT: u64 = 1;
const SAFE: Address = address!("5637a7552003411c8953Feba6E38951a32Ab9c49");
const SAFE_SINGLETON: Address = address!("41675c099f32341bf84bfc5382af534df5c7461a");
const MULTISEND: Address = address!("40A2aCCbd92BCA938b02010E17A5b8929b49130D");

struct Relay;

impl Tile<BuilderSpine> for Relay {
    fn on_attach(&mut self, adapter: &mut SpineAdapter<BuilderSpine>) {
        let outputs: &mut SpineDCacheConsumer<FromEngine> = adapter.consumers.as_mut();
        outputs.inner.subscribe_broadcast();
    }

    fn loop_body(&mut self, _adapter: &mut SpineAdapter<BuilderSpine>) {}
}

enum Output {
    Merged(Box<MergedBlockV2>),
    Reject(RejectV1),
}

struct Harness {
    adapter: SpineAdapter<BuilderSpine>,
    stop: Arc<AtomicBool>,
    engine: Option<JoinHandle<()>>,
    _spine: TestSpine,
}

impl Harness {
    fn start(mut engine: MergeEngine) -> Self {
        let mut spine = TestSpine::new();
        let mut engine_adapter = SpineAdapter::connect_tile(&engine, &mut spine.spine);
        engine.on_attach(&mut engine_adapter);
        let mut adapter = SpineAdapter::connect_tile(&Relay, &mut spine.spine);
        Relay.on_attach(&mut adapter);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let run = move || {
            while !stopped.load(Ordering::Relaxed) {
                engine.loop_body(&mut engine_adapter);
            }
            engine.teardown(&mut engine_adapter);
        };
        let engine = std::thread::Builder::new().stack_size(STACK_SIZE).spawn(run).unwrap();
        Self { adapter, stop, engine: Some(engine), _spine: spine }
    }

    fn send(&mut self, kind: EventKind, slot: u64, body: &[u8]) {
        let event = ToEngine {
            kind,
            _pad: [0; 7],
            generation: 0,
            slot,
            recv_ns: utcnow_ns(),
            seq: 0,
            charge: 0,
        };
        let write = |buf: &mut [u8]| buf.copy_from_slice(body);
        let payload = (!body.is_empty()).then_some((body.len(), write));
        self.adapter.produce_with_dcache(event, payload).unwrap();
    }

    fn block(&mut self, msg: &MergeableBlockV1) {
        self.send(EventKind::MergeableBlock, msg.slot, &msg.as_ssz_bytes());
    }

    fn drain(&mut self, outputs: &mut Vec<Output>) {
        self.adapter.consume_with_dcache(
            |_: FromEngine, body: &[u8]| body.to_vec(),
            |read, _| {
                let DCacheRead::Ok((output, body)) = read else { panic!("engine output lost") };
                outputs.push(match output.kind {
                    OutputKind::Merged => {
                        let msg = MergedBlockV1::from_ssz_bytes(&body).unwrap();
                        Output::Merged(Box::new(msg.into()))
                    }
                    OutputKind::MergedV2 => {
                        Output::Merged(Box::new(MergedBlockV2::from_ssz_bytes(&body).unwrap()))
                    }
                    OutputKind::Reject => Output::Reject(RejectV1::from_ssz_bytes(&body).unwrap()),
                });
            },
        );
    }

    fn settle(&mut self, quiet: Duration) -> Vec<Output> {
        let mut outputs = Vec::new();
        let mut last = Instant::now();
        while last.elapsed() < quiet {
            let seen = outputs.len();
            self.drain(&mut outputs);
            if outputs.len() > seen {
                last = Instant::now();
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        outputs
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(engine) = self.engine.take() {
            let _ = engine.join();
        }
    }
}

struct Fixture {
    store: Store,
    blockchain: Arc<Blockchain>,
    genesis_hash: ethrex_common::H256,
    genesis_timestamp: u64,
    amsterdam: bool,
    chain_id: u64,
    gas_limit: u64,
    signers: Vec<PrivateKeySigner>,
    proposer: Address,
    block_value: U256,
    relay_config: RelayConfigV1,
}

impl Fixture {
    /// Payees exist already: under Amsterdam a first payment to a new account needs far
    /// more than a transfer's 21k gas.
    async fn amsterdam() -> Self {
        Self::new(|genesis| {
            genesis.config.amsterdam_time = Some(0);
            deploy_amsterdam_predeploys(genesis);
            for payee in [Address::repeat_byte(0x77), Address::repeat_byte(0x55)] {
                genesis.alloc.insert(eaddr(payee), GenesisAccount {
                    code: Default::default(),
                    storage: Default::default(),
                    balance: ethrex_common::U256::one(),
                    nonce: 0,
                });
            }
        })
        .await
    }

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
            amsterdam: genesis.config.is_amsterdam_activated(block.header.timestamp),
            chain_id: genesis.config.chain_id,
            gas_limit: block.header.gas_limit,
            signers,
            proposer: Address::repeat_byte(0x77),
            block_value: U256::from(ETH / 2),
            relay_config,
        }
    }

    fn start(&self) -> Harness {
        let config = EngineConfig {
            relay_signer: self.signers[6].clone(),
            max_blocks_per_slot: 64,
            max_orders_per_slot: 1024,
            max_builders: 8,
            min_value_increase_wei: U256::ZERO,
            max_base_age: Duration::from_secs(5),
            warmup: false,
            blacklist: Default::default(),
            record: None,
        };
        let mut harness =
            Harness::start(MergeEngine::new(config, self.store.clone(), Default::default()));
        let slot_start = SlotStartV1 {
            slot: SLOT,
            parent_hash: b256(self.genesis_hash),
            proposer_fee_recipient: self.proposer,
            parent_beacon_block_root: B256::ZERO,
            apply_blacklist: false,
        };
        harness.send(EventKind::RelayConfig, 0, &self.relay_config.as_ssz_bytes());
        harness.send(EventKind::SlotStart, SLOT, &slot_start.as_ssz_bytes());
        harness
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
            slot_number: self.amsterdam.then_some(SLOT),
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

    fn assert_valid(&self, merged: &MergedBlockV2) {
        let block = block_of(merged, B256::ZERO, SLOT);
        self.blockchain.add_block(block).expect("merged block must import");
    }
}

/// The merged block as ethrex imports it; an empty access list means a pre-Amsterdam block.
fn block_of(merged: &MergedBlockV2, beacon_root: B256, slot: u64) -> ethrex_common::types::Block {
    let encoded: Vec<_> = merged
        .execution_requests
        .iter()
        .map(|r| ethrex_common::types::requests::EncodedRequests(r.clone().into()))
        .collect();
    let decoded = decode_execution_requests(&encoded).unwrap();
    let requests = helix_types::requests_to_v4(&decoded.requests);
    let amsterdam = (!merged.block_access_list.is_empty()).then_some(Amsterdam {
        block_access_list: &merged.block_access_list,
        slot,
        builder_deposits: Some(&decoded.builder_deposits),
        builder_exits: Some(&decoded.builder_exits),
    });
    payload_v3_to_block(&merged.execution_payload, beacon_root, &requests, amsterdam).unwrap()
}

fn ranked_base(pool: &mut Pool, pubkey: u8, value: u64) -> BaseBlock {
    let pubkey = BlsPublicKey::repeat_byte(pubkey);
    let submitter = match pool.submitters.iter().position(|s| s.pubkey == pubkey) {
        Some(i) => i as u16,
        None => {
            pool.submitters.push(Submitter { pubkey, address: Address::ZERO });
            (pool.submitters.len() - 1) as u16
        }
    };
    pool.block_values.push(U256::from(value));
    let block = (pool.block_values.len() - 1) as u32;
    BaseBlock {
        hash: B256::with_last_byte(block as u8 + 1),
        block,
        submitter,
        ..Default::default()
    }
}

fn builder(coinbase: u8) -> Builder {
    Builder {
        coinbase: Address::repeat_byte(coinbase),
        bases: Vec::new(),
        merger: BlockMerger::new(),
    }
}

#[test]
fn the_highest_base_per_pubkey_is_served_first() {
    let mut pool = Pool::default();
    let mut builders = vec![builder(1), builder(2)];
    let a1 = ranked_base(&mut pool, 0xa1, 10);
    builders[0].upsert_base(a1, &pool.submitters);
    let b = ranked_base(&mut pool, 0xb0, 20);
    builders[1].upsert_base(b, &pool.submitters);
    assert_eq!(select_base(&builders, &pool, 0, u64::MAX), Some((1, 0)));

    let a2 = ranked_base(&mut pool, 0xa2, 30);
    builders[0].upsert_base(a2, &pool.submitters);
    assert_eq!(select_base(&builders, &pool, 0, u64::MAX), Some((0, 1)));

    let cancel = ranked_base(&mut pool, 0xa2, 5);
    builders[0].upsert_base(cancel, &pool.submitters);
    assert_eq!(builders[0].bases.len(), 2);
    assert_eq!(builders[0].top_base(&pool.block_values), Some(0));
    assert_eq!(select_base(&builders, &pool, 0, u64::MAX), Some((1, 0)));
}

#[test]
fn an_opened_top_without_work_yields() {
    let mut pool = Pool::default();
    let mut builders = vec![builder(1), builder(2)];
    let low = ranked_base(&mut pool, 0xa0, 10);
    builders[0].upsert_base(low, &pool.submitters);
    let high = ranked_base(&mut pool, 0xb0, 20);
    let mut merger = BlockMerger::new();
    merger.base.copy_from(&high);
    merger.live = true;
    merger.seen_version = pool.orders_version;
    builders[1].upsert_base(high, &pool.submitters);
    builders[1].merger = merger;
    assert_eq!(select_base(&builders, &pool, 0, u64::MAX), Some((0, 0)));

    pool.orders_version += 1;
    assert_eq!(select_base(&builders, &pool, 0, u64::MAX), Some((1, 0)));

    let age = builders[1].merger.base.recv_ns + 1;
    assert_eq!(select_base(&builders, &pool, age, 1), Some((0, 0)));
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

fn best(engine: &mut Harness) -> Box<MergedBlockV2> {
    // In a debug build under CPU contention, the first merge can take longer
    // than one quiet window.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut outputs = Vec::new();
    while outputs.is_empty() && Instant::now() < deadline {
        outputs = engine.settle(Duration::from_secs(3));
    }
    let mut best: Option<Box<MergedBlockV2>> = None;
    for output in outputs {
        let msg = match output {
            Output::Merged(msg) => msg,
            Output::Reject(msg) => {
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
    merges_an_order(Fixture::new(|_| {}).await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn merges_an_order_into_a_valid_amsterdam_block() {
    merges_an_order(Fixture::amsterdam().await).await;
}

/// The second base pays the order's sender first, so the order replays its first run's
/// effect from a different pre-balance; the access list must still carry the right
/// post-balances.
#[tokio::test(flavor = "multi_thread")]
async fn a_replayed_order_keeps_a_valid_amsterdam_access_list() {
    let fixture = Fixture::amsterdam().await;
    let base = fixture.base();
    let tx = fixture.transfer(3, fixture.signers[0].address(), U256::from(ETH / 5));
    let order = fixture.orders(&base, 1, vec![tx], vec![tx_order(0)]);
    let mut engine = fixture.start();
    engine.block(&base);
    engine.block(&order);
    let first = best(&mut engine);
    assert_eq!(first.included_order_ids.len(), 1);

    let topup = fixture.transfer(1, fixture.signers[3].address(), U256::from(ETH));
    let payment = signed_transfer(
        &fixture.signers[0],
        fixture.chain_id,
        0,
        fixture.proposer,
        fixture.block_value + U256::from(1),
        100 * GWEI,
        0,
    );
    let mut richer = fixture.base_from_txs(vec![topup, payment]);
    richer.block_value = fixture.block_value + U256::from(1);
    engine.block(&richer);
    let merged = best(&mut engine);
    let richer_hash = richer.execution_payload.payload_inner.payload_inner.block_hash;
    assert_eq!(merged.base_block_hash, richer_hash);
    assert_eq!(merged.included_order_ids.len(), 1);
    assert!(!merged.block_access_list.is_empty());
    fixture.assert_valid(&merged);
}

async fn merges_an_order(fixture: Fixture) {
    let base = fixture.base();
    let value = U256::from(ETH / 5);
    let tx = fixture.transfer(3, fixture.signers[0].address(), value);
    let order = fixture.orders(&base, 1, vec![tx], vec![tx_order(0)]);
    let mut engine = fixture.start();
    engine.block(&base);
    engine.block(&order);

    let merged = best(&mut engine);
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
    assert_eq!(merged.block_access_list.is_empty(), !fixture.amsterdam);
    fixture.assert_valid(&merged);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_builder_reused_across_slots_merges_a_valid_block() {
    let fixture = Fixture::new(|_| {}).await;
    let base = fixture.base();
    let tx = fixture.transfer(3, fixture.signers[0].address(), U256::from(ETH / 5));
    let order = fixture.orders(&base, 1, vec![tx], vec![tx_order(0)]);
    let mut engine = fixture.start();
    engine.block(&base);
    engine.block(&order);
    best(&mut engine);

    // A new parent for the same slot ends it; switching back starts a fresh slot.
    let mut slot_start = SlotStartV1 {
        slot: SLOT,
        parent_hash: B256::repeat_byte(1),
        proposer_fee_recipient: fixture.proposer,
        parent_beacon_block_root: B256::ZERO,
        apply_blacklist: false,
    };
    engine.send(EventKind::SlotStart, SLOT, &slot_start.as_ssz_bytes());
    slot_start.parent_hash = b256(fixture.genesis_hash);
    engine.send(EventKind::SlotStart, SLOT, &slot_start.as_ssz_bytes());
    engine.block(&base);
    engine.block(&order);

    let merged = best(&mut engine);
    assert_eq!(merged.included_order_ids.len(), 1);
    fixture.assert_valid(&merged);
}

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
    let mut engine = fixture.start();
    engine.block(&base);
    engine.block(&order);

    let merged = best(&mut engine);
    assert!(merged.proposer_value > fixture.block_value);
    fixture.assert_valid(&merged);
}

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

#[tokio::test(flavor = "multi_thread")]
async fn distributes_through_the_collateral_safe_after_a_safe_paid_base() {
    let mut fixture = Fixture::new(deploy_collateral_safe).await;
    fixture.relay_config.multisend_contract = MULTISEND;
    fixture.relay_config.builder_collaterals[0].collateral_safe = SAFE;

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
    let mut engine = fixture.start();
    engine.block(&order);
    engine.block(&base);

    let merged = best(&mut engine);
    assert_eq!(merged.included_order_ids.len(), 1);
    fixture.assert_valid(&merged);

    let v1 = &merged.execution_payload.payload_inner.payload_inner;
    let last = v1.transactions.last().unwrap();
    let distribution = ethrex_common::types::Transaction::decode_canonical(last).unwrap();
    let cost = v1.base_fee_per_gas * U256::from(fixture.relay_config.distribution_gas_limit);
    assert!(!cost.is_zero());
    let refund = multisend_paid_amount(distribution.data(), fixture.signers[6].address());
    assert_eq!(refund, cost);
}

#[tokio::test(flavor = "multi_thread")]
async fn drops_a_droppable_tx_whose_gas_limit_no_longer_fits() {
    let burner = Address::repeat_byte(0x66);
    let fixture = Fixture::new(|genesis| {
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
    let mut engine = fixture.start();
    for msg in [&filler, &late, &base] {
        engine.block(msg);
    }

    let merged = best(&mut engine);
    assert_eq!(merged.included_order_ids.len(), 2);
    let txs = &merged.execution_payload.payload_inner.payload_inner.transactions;
    assert!(txs.iter().all(|tx| keccak256(tx) != wide_hash));
    fixture.assert_valid(&merged);
}

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
    let mut engine = fixture.start();
    for msg in [&older, &newer, &base] {
        engine.block(msg);
    }

    let merged = best(&mut engine);
    assert_eq!(merged.included_order_ids, vec![order_id(kept, &BlsPublicKey::repeat_byte(1))]);
    fixture.assert_valid(&merged);
}

#[tokio::test]
async fn a_rejected_block_still_caches_its_own_txs() {
    let fixture = Fixture::new(|_| {}).await;
    let base = fixture.base();
    let own = fixture.transfer(3, Address::repeat_byte(0x55), U256::from(1));
    let parent = b256(fixture.genesis_hash);
    let mut pool = Pool::default();

    let unknown = B256::repeat_byte(0xab).to_vec();
    let failing = fixture.orders(&base, 1, vec![own.clone(), unknown], vec![tx_order(0)]);
    let added = pool.add(&failing.as_ssz_bytes(), 0, SLOT, parent, 64, 64);
    let Err(reject) = added else { panic!("an unresolved reference rejects the block") };
    assert_eq!(reject.code, RejectCode::InvalidOrder);
    assert!(reject.msg.starts_with("unresolved tx hash reference"), "{}", reject.msg);

    let by_ref = keccak256(&own).to_vec();
    let referencing = fixture.orders(&base, 2, vec![by_ref], vec![tx_order(0)]);
    let added = pool.add(&referencing.as_ssz_bytes(), 0, SLOT, parent, 64, 64);
    assert!(added.is_ok_and(|added| added.orders_dropped.is_none()));
    assert_eq!(pool.orders.len(), 1);

    let other = fixture.transfer(4, Address::repeat_byte(0x55), U256::from(1));
    let wrong_parent = fixture.orders(&base, 3, vec![other.clone()], vec![tx_order(0)]);
    let added = pool.add(&wrong_parent.as_ssz_bytes(), 0, SLOT, B256::ZERO, 64, 64);
    let Err(reject) = added else { panic!("a wrong parent rejects the block") };
    assert_eq!(reject.code, RejectCode::HeadMismatch);

    let by_ref = keccak256(&other).to_vec();
    let referencing = fixture.orders(&base, 4, vec![by_ref], vec![tx_order(0)]);
    let added = pool.add(&referencing.as_ssz_bytes(), 0, SLOT, parent, 64, 64);
    assert!(added.is_ok_and(|added| added.orders_dropped.is_none()));
    assert_eq!(pool.orders.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
// REPLAY_DIR=<record dir>/<slot> cargo test --release -p helix-builder replay_recording --
// --ignored --nocapture
async fn replay_recording() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_default())
        .try_init();
    ethrex_crypto::kzg::warm_up_trusted_setup();
    let dir = std::path::PathBuf::from(std::env::var("REPLAY_DIR").expect("REPLAY_DIR"));
    let (outputs, validator, beacon_root, slot) = replay::slot(&dir).await;

    let mut first: rustc_hash::FxHashMap<B256, (u64, u64)> = Default::default();
    let mut per_base: std::collections::BTreeMap<B256, (U256, B256, usize)> = Default::default();
    let mut rejects: std::collections::BTreeMap<String, usize> = Default::default();
    let mut merged = 0;
    let mut best = U256::ZERO;
    for output in &outputs {
        match output {
            Output::Merged(msg) => {
                merged += 1;
                best = best.max(msg.proposer_value);
                let t = msg.trace;
                first.entry(msg.base_block_hash).or_insert((t.base_block_recv_ns, t.finalize_ns));
                let txs = msg.execution_payload.payload_inner.payload_inner.transactions.len();
                let hash = msg.execution_payload.payload_inner.payload_inner.block_hash;
                let entry = per_base.entry(msg.base_block_hash).or_insert((U256::ZERO, hash, txs));
                if msg.proposer_value > entry.0 {
                    *entry = (msg.proposer_value, hash, txs);
                }
            }
            Output::Reject(msg) => {
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
    println!("best proposer_value={best}");
    println!("first emit p50 {:.2}ms p90 {:.2}ms max {:.2}ms", q(0.5), q(0.9), q(1.0));
    for (base, (value, hash, txs)) in &per_base {
        println!("base={base:#x} best={value} merged={hash:#x} txs={txs}");
    }

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
                // Upper bound of the bucket holding the quantile, as read off a scrape.
                let q = |p: f64| {
                    let want = (n as f64 * p).ceil() as u64;
                    let bucket = h.get_bucket().iter().find(|b| b.get_cumulative_count() >= want);
                    bucket.map_or(f64::INFINITY, |b| b.get_upper_bound() / 1e3)
                };
                println!(
                    "stage {stage:<18} n={n:<6} mean {mean_ms:>8.2}ms p50<={:.2}ms p90<={:.2}ms p99<={:.2}ms",
                    q(0.5),
                    q(0.9),
                    q(0.99)
                );
            }
        }
    }

    if let Some(every) = std::env::var("REPLAY_VALIDATE").ok().and_then(|v| v.parse::<usize>().ok())
    {
        let (mut ok, mut failed) = (0, std::collections::BTreeMap::<String, usize>::new());
        let merged = outputs.iter().filter_map(|output| match output {
            Output::Merged(msg) => Some(msg),
            Output::Reject(_) => None,
        });
        for msg in merged.step_by(every.max(1)) {
            match validator.add_block(block_of(msg, beacon_root, slot)) {
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

    use super::{Harness, Output};
    use crate::{
        convert::{b256, eaddr},
        merge::{
            EngineConfig, MergeEngine,
            record::{RecordMeta, SAFE_SENTINEL, TrieProofs, replay_signer, safe_owner_slot},
        },
        spine::EventKind,
        testing::ETH,
    };

    const RELAY_CONFIG: u8 = EventKind::RelayConfig as u8;
    const SLOT_START: u8 = EventKind::SlotStart as u8;
    const SLOT_END: u8 = EventKind::SlotEnd as u8;
    const MERGEABLE: u8 = EventKind::MergeableBlock as u8;

    pub async fn slot(dir: &Path) -> (Vec<Output>, Arc<Blockchain>, B256, u64) {
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

        let signer = replay_signer();
        let replay_owner = eaddr(signer.address());
        let word =
            |a: Address| U256::from_big_endian(B256::left_padding_from(a.as_bytes()).as_slice());
        let owner_slot = |owner: Address| U256::from_big_endian(safe_owner_slot(owner).as_bytes());
        let relay_config = frames
            .iter()
            .find(|(kind, ..)| *kind == RELAY_CONFIG)
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
            blacklist: Default::default(),
            record: None,
        };
        let mut engine = Harness::start(MergeEngine::new(config, store, Default::default()));
        let mut outputs = Vec::new();

        let mut beacon_root = B256::ZERO;
        let recorded_start = frames[0].1;
        let start = std::time::Instant::now();
        let mut replayed = None;
        for (kind, ns, body) in frames {
            let due = Duration::from_nanos(ns.saturating_sub(recorded_start));
            while let Some(wait) = due.checked_sub(start.elapsed()) {
                engine.drain(&mut outputs);
                std::thread::sleep(wait.min(Duration::from_millis(1)));
            }
            match kind {
                RELAY_CONFIG => engine.send(EventKind::RelayConfig, 0, &body),
                SLOT_START => {
                    if replayed.is_some() {
                        break;
                    }
                    let mut body = body;
                    body.resize(body.len().max(93), 0);
                    let mut msg = SlotStartV1::from_ssz_bytes(&body).unwrap();
                    replayed = Some(msg.slot);
                    beacon_root = msg.parent_beacon_block_root;
                    msg.parent_hash = b256(parent_hash);
                    engine.send(EventKind::SlotStart, msg.slot, &msg.as_ssz_bytes());
                }
                MERGEABLE => {
                    let mut msg = MergeableBlockV1::from_ssz_bytes(&body).unwrap();
                    msg.execution_payload.payload_inner.payload_inner.parent_hash =
                        b256(parent_hash);
                    engine.block(&msg);
                }
                SLOT_END => break,
                _ => continue,
            }
        }
        outputs.extend(engine.settle(Duration::from_millis(600)));
        if let Some(slot) = replayed {
            engine.send(EventKind::SlotEnd, slot, &[]);
        }
        outputs.extend(engine.settle(Duration::from_millis(100)));
        (outputs, blockchain, beacon_root, replayed.unwrap_or_default())
    }

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

mod effects {
    use ethrex_common::{
        Address, Bytes, H256,
        types::{Log, Receipt, TxType, bloom_from_logs},
    };
    use ethrex_crypto::native::NativeCrypto;
    use ethrex_rlp::encode::RLPEncode;

    use crate::merge::effects::put_receipt;

    #[test]
    fn put_receipt_matches_ethrex() {
        let log = |n: u8, data: usize| Log {
            address: Address::repeat_byte(n),
            topics: (0..n % 5).map(|t| H256::repeat_byte(t)).collect(),
            data: Bytes::from(vec![n; data]),
        };
        let logs = [vec![], vec![log(1, 0)], vec![log(2, 31), log(3, 300), log(4, 70_000)]];
        let types =
            [TxType::Legacy, TxType::EIP2930, TxType::EIP1559, TxType::EIP4844, TxType::EIP7702];
        for (tx_type, logs) in types.into_iter().flat_map(|t| logs.iter().map(move |l| (t, l))) {
            for (succeeded, gas) in
                [(true, 0), (false, 1), (true, 127), (true, 128), (false, 29_999_999)]
            {
                let receipt = Receipt::new(tx_type, succeeded, gas, logs.clone());
                let bloom = bloom_from_logs(&receipt.logs, &NativeCrypto);
                let mut out = Vec::new();
                let logs = receipt.logs.encode_to_vec();
                put_receipt(&mut out, &receipt, receipt.cumulative_gas_used, &bloom, &logs);
                assert_eq!(out, receipt.encode_inner_with_precomputed_bloom(bloom));
            }
        }
    }
}

mod keccak {
    use ethrex_crypto::keccak::keccak_hash;

    use crate::merge::keccak::hash_many;

    #[test]
    fn matches_keccak() {
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i * 31 + i / 7) as u8).collect();
        let mut spans = Vec::new();
        let mut at = 0u32;
        for len in (0..700u32).chain([135, 136, 137, 272, 4000, 1, 0, 9000]) {
            spans.push((0, at, len));
            at += len + 3;
        }
        for n in [1, 2, 3, 8, 9, spans.len()] {
            let mut out = vec![[0u8; 32]; n];
            hash_many(&bytes, &spans[..n], &mut out);
            for (&(_, at, len), got) in spans.iter().zip(&out) {
                assert_eq!(
                    *got,
                    keccak_hash(&bytes[at as usize..(at + len) as usize]),
                    "len={len}"
                );
            }
        }
    }
}

mod lists {
    use ethrex_crypto::native::NativeCrypto;
    use ethrex_rlp::encode::RLPEncode;
    use ethrex_trie::Trie;

    use crate::merge::lists::ListRoot;

    #[test]
    fn matches_the_trie_root() {
        let mut lists = ListRoot::default();
        let values: Vec<Vec<u8>> = (0..1100u32)
            .map(|i| (0..(i * 37 % 300) as u8).map(|b| b ^ i as u8).collect())
            .collect();
        for n in (0..300).chain([1000, 1100]) {
            let leaves = (0..n).map(|i| (i.encode_to_vec(), values[i].clone()));
            let want = Trie::compute_hash_from_unsorted_iter(leaves, &NativeCrypto);
            assert_eq!(lists.compute(n, |i| &values[i]), want, "n={n}");
        }
    }
}

mod payment {
    use alloy_primitives::{Address, U256, address};
    use alloy_signer_local::PrivateKeySigner;
    use helix_tcp_types::merging::control::RelayConfigV1;

    use crate::merge::{
        payment::{Revenue, SafePayment, payment_tx, split_revenue},
        pool::Submitter,
    };

    fn relay(relay_fee_recipient: Address) -> RelayConfigV1 {
        RelayConfigV1 {
            relay_fee_recipient,
            multisend_contract: Address::ZERO,
            relay_bps: 2500,
            merged_builder_bps: 2500,
            winning_builder_bps: 2500,
            distribution_gas_limit: 140_000,
            builder_collaterals: Vec::new(),
        }
    }

    const ORIGINS: [Address; 2] = [
        address!("0000000000000000000000000000000000000006"),
        address!("0000000000000000000000000000000000000007"),
    ];

    fn submitters() -> Vec<Submitter> {
        ORIGINS.iter().map(|&address| Submitter { pubkey: Default::default(), address }).collect()
    }

    fn revenues(paid: [u64; 2]) -> Vec<Revenue> {
        (0..2).map(|i| Revenue { submitter: i as u16, paid: U256::from(paid[i]) }).collect()
    }

    fn amount_to(split: &[(Address, U256)], to: Address) -> Option<U256> {
        split.iter().find(|(a, _)| *a == to).map(|(_, v)| *v)
    }

    #[test]
    fn split_matches_relay_accounting() {
        let (winner, relay_to, proposer, payer) = (
            address!("0000000000000000000000000000000000000001"),
            address!("0000000000000000000000000000000000000002"),
            address!("0000000000000000000000000000000000000003"),
            address!("0000000000000000000000000000000000000004"),
        );
        let revs = revenues([10_000, 30_000]);
        let out = split_revenue(
            &relay(relay_to),
            &revs,
            &submitters(),
            U256::ZERO,
            payer,
            proposer,
            winner,
        );
        assert_eq!(out.iter().map(|(_, v)| *v).sum::<U256>(), U256::from(30_000));
        assert_eq!(amount_to(&out, relay_to), Some(U256::from(10_000)));
        assert_eq!(amount_to(&out, proposer), Some(U256::from(10_000)));
        assert_eq!(amount_to(&out, ORIGINS[0]), Some(U256::from(2_500)));
        assert_eq!(amount_to(&out, ORIGINS[1]), Some(U256::from(7_500)));
        assert_eq!(amount_to(&out, winner), None);
        assert_eq!(amount_to(&out, payer), None);

        let cost = U256::from(4_000);
        let out =
            split_revenue(&relay(relay_to), &revs, &submitters(), cost, payer, proposer, winner);
        assert_eq!(out.iter().map(|(_, v)| *v).sum::<U256>(), U256::from(31_000));
        assert_eq!(amount_to(&out, payer), Some(cost));
        assert_eq!(amount_to(&out, proposer), Some(U256::from(9_000)));
        assert_eq!(amount_to(&out, relay_to), Some(U256::from(9_000)));
        assert_eq!(amount_to(&out, ORIGINS[0]), Some(U256::from(2_250)));
        assert_eq!(amount_to(&out, ORIGINS[1]), Some(U256::from(6_750)));

        let revs = revenues([7, 5]);
        let out = split_revenue(
            &relay(relay_to),
            &revs,
            &submitters(),
            U256::ZERO,
            payer,
            proposer,
            winner,
        );
        assert_eq!(out.iter().map(|(_, v)| *v).sum::<U256>(), U256::from(8));
        assert_eq!(amount_to(&out, proposer), Some(U256::from(3)));
        assert_eq!(amount_to(&out, relay_to), Some(U256::from(3)));
        assert_eq!(amount_to(&out, ORIGINS[0]), Some(U256::from(1)));
        assert_eq!(amount_to(&out, ORIGINS[1]), Some(U256::from(1)));
    }

    #[test]
    fn payment_tx_decodes_as_signed_by_the_relay() {
        let signer: PrivateKeySigner =
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".parse().unwrap();
        let p = SafePayment {
            safe: address!("1111111111111111111111111111111111111111"),
            safe_balance: U256::from(10_000_000u64),
            safe_nonce: 3,
            signer_nonce: 7,
            chain_id: 1,
            gas_limit: 140_000,
            base_fee: 1_000_000_000,
            multisend: address!("2222222222222222222222222222222222222222"),
        };
        let to = [(address!("3333333333333333333333333333333333333333"), U256::from(1_000u64))];
        let encoded = payment_tx(&signer, &p, &to).unwrap();
        let tx = ethrex_common::types::Transaction::decode_canonical(&encoded).unwrap();
        assert_eq!(tx.gas_limit(), 140_000);
        assert_eq!(tx.nonce(), 7);
        let sender = tx.sender(&ethrex_crypto::native::NativeCrypto).unwrap();
        assert_eq!(sender, crate::convert::eaddr(signer.address()));

        let broke = SafePayment { safe_balance: U256::from(999u64), ..p };
        assert_eq!(payment_tx(&signer, &broke, &to), Err("safe_balance"));
    }
}

mod state {
    use ethrex_common::U256;

    use crate::merge::state::UndoTable;

    #[test]
    fn undo_restores_values_and_written() {
        let mut t = UndoTable::<U256>::new(16);
        assert!(t.set(3, U256::from(1)));
        let at = t.undo_log.len() as u32;
        assert!(t.set(3, U256::from(2)));
        assert!(t.set(5, U256::from(7)));
        assert_eq!((t.get(3), t.get(5)), (Some(U256::from(2)), Some(U256::from(7))));
        t.undo_to(at);
        assert_eq!((t.get(3), t.get(5), &t.written[..]), (Some(U256::from(1)), None, &[3][..]));
        t.undo_log.clear();
        assert!(t.set(5, U256::from(8)));
        t.undo_to(0);
        assert_eq!((t.get(5), &t.written[..]), (None, &[3][..]));
        t.reset();
        assert_eq!((t.get(3), t.written.len(), t.undo_log.len()), (None, 0, 0));
    }
}

mod trie {
    use std::collections::BTreeMap;

    use ethrex_crypto::{keccak::keccak_hash, native::NativeCrypto};
    use ethrex_trie::{InMemoryTrieDB, Trie, db::NodeMap};

    use crate::merge::trie::{NodeArena, TrieHasher, Tries};

    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn check_against_ethrex(
        key_of: impl Fn(u64) -> [u8; 32],
        keys: u64,
        value_max: u64,
        rng: &mut XorShift,
    ) {
        let value = |rng: &mut XorShift| -> Vec<u8> {
            (0..1 + rng.next() % value_max).map(|_| rng.next() as u8).collect()
        };
        let mut model = BTreeMap::new();
        for k in 0..keys / 2 {
            model.insert(key_of(k), value(rng));
        }
        let (mut parent, mut own) =
            (NodeArena::new(false, 1 << 15, 1 << 18), NodeArena::new(true, 1 << 14, 1 << 16));
        let (mut hasher, mut out) = (TrieHasher::default(), Vec::new());
        for n in 0..3 {
            let map = NodeMap::default();
            let mut want = Trie::new(Box::new(InMemoryTrieDB::new(map.clone())));
            for (k, v) in &model {
                want.insert(k.to_vec(), v.clone()).unwrap();
            }
            want.commit(&NativeCrypto).unwrap();
            let db = InMemoryTrieDB::new(map);
            parent.trim();
            own.clear();
            let mut tries = Tries { parent: &mut parent, own: &mut own };
            let mut root = tries.open_root(want.hash_no_commit(&NativeCrypto));
            for round in 0..8 {
                for _ in 0..200 {
                    let key = key_of(rng.next() % keys);
                    let v = (rng.next() % 8 >= round).then(|| value(rng));
                    match &v {
                        Some(v) => {
                            want.insert(key.to_vec(), v.clone()).unwrap();
                            model.insert(key, v.clone());
                        }
                        None => {
                            want.remove(&key).unwrap();
                            model.remove(&key);
                        }
                    }
                    root = tries.put(&db, root, &key, v.as_deref()).unwrap();
                }
                tries.commit(&mut hasher, &[root], &mut out);
                assert_eq!(out[0], want.hash_no_commit(&NativeCrypto), "parent {n} round {round}");
            }
        }
        assert!(!parent.loaded.is_empty(), "trim kept the parent arena");
    }

    #[test]
    fn matches_the_trie_root() {
        let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
        check_against_ethrex(|k| keccak_hash(k.to_be_bytes()), 2000, 110, &mut rng);
        check_against_ethrex(
            |k| std::array::from_fn(|i| if i == 31 { k as u8 } else { 7 }),
            256,
            3,
            &mut rng,
        );
    }
}
