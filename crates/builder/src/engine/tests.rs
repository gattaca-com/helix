use std::{str::FromStr, sync::Arc, time::Duration};

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use ethrex_blockchain::{
    Blockchain, BlockchainOptions, BlockchainType,
    payload::{BuildPayloadArgs, create_payload},
};
use ethrex_common::types::ELASTICITY_MULTIPLIER;
use ethrex_config::networks::Network;
use ethrex_storage::{EngineType, Store};
use helix_tcp_types::merging::{
    builder_to_relay::RejectCode,
    control::{BuilderCollateral, RelayConfigV1},
    order::{MergeOrderRef, TxOrderRef},
    relay_to_builder::{MergeableBlockV1, SlotStartV1},
};
use ssz::Encode;
use tokio::sync::watch;

use crate::{
    engine::{
        EngineEvent, EngineOutput, MergeEngine,
        convert::{aaddr, b256, block_to_payload_v3, eaddr},
        types::EngineConfig,
    },
    node::HeadInfo,
};

const KEYS: [&str; 20] = [
    "0x941e103320615d394a55708be13e45994c7d93b932b064dbcb2b511fe3254e2e",
    "0xbcdf20249abf0ed6d944c0288fad489e33f66b3960d9e6229c1cd214ed3bbe31",
    "0x39725efee3fb28614de3bacaffe4cc4bd8c436257e2c8bb887c4b5c4be45e76d",
    "0x53321db7c1e331d93a11a41d16f004d7ff63972ec8ec7c25db329728ceeb1710",
    "0xab63b23eb7941c1251757e24b3d2350d2bc05c3c388d06f8fe6feafefb1e8c70",
    "0x5d2344259f42259f82d2c140aa66102ba89b57b4883ee441a8b312622bd42491",
    "0x27515f805127bebad2fb9b183508bdacb8c763da16f54e0678b16e8f28ef3fff",
    "0x7ff1a4c1d57e5e784d327c4c7651e952350bc271f156afb3d00d20f5ef924856",
    "0x3a91003acaf4c21b3953d94fa4a6db694fa69e5242b2e37be05dd82761058899",
    "0xbb1d0f125b4fb2bb173c318cdead45468474ca71474e2247776b2b4c0fa2d3f5",
    "0x850643a0224065ecce3882673c21f56bcf6eef86274cc21cadff15930b59fc8c",
    "0x94eb3102993b41ec55c241060f47daa0f6372e2e3ad7e91612ae36c364042e44",
    "0xdaf15504c22a352648a71ef2926334fe040ac1d5005019e09f6c979808024dc7",
    "0xeaba42282ad33c8ef2524f07277c03a776d98ae19f581990ce75becb7cfa1c23",
    "0x3fd98b5187bf6526734efaa644ffbb4e3670d66f5d0268ce0323ec09124bff61",
    "0x5288e2f440c7f0cb61a9be8afdeb4295f786383f96f5e35eb0c94ef103996b64",
    "0xf296c7802555da2a5a662be70e078cbd38b44f96f8615ae529da41122ce8db05",
    "0xbf3beef3bd999ba9f2451e06936f0423cd62b815c9233dd3bc90f7e02a1e8673",
    "0x6ecadc396415970e91293726c3f5775225440ea0844ae5616135fd10d66b5954",
    "0xa492823c3e193d6c595f37a18e3c06650cf4c74558cc818b16130b293716106f",
];

const GWEI: u128 = 1_000_000_000;
const ETH: u128 = 1_000_000_000_000_000_000;
const SLOT: u64 = 1;

#[allow(clippy::too_many_arguments)]
fn signed_transfer(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    to: Address,
    value: U256,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> Vec<u8> {
    let tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        to: to.into(),
        value,
        access_list: Default::default(),
        input: Default::default(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    alloy_consensus::TxEnvelope::from(tx.into_signed(signature)).encoded_2718()
}

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
    async fn new() -> Self {
        let genesis = Network::LocalDevnet.get_genesis().unwrap();
        let chain_id = genesis.config.chain_id;
        let genesis_block = genesis.get_block();
        let genesis_hash = genesis_block.hash();
        let genesis_header = genesis_block.header.clone();

        let mut store = Store::new("memory", EngineType::InMemory).unwrap();
        store.add_initial_state(genesis.clone()).await.unwrap();
        let blockchain: Arc<Blockchain> = Blockchain::new(store.clone(), BlockchainOptions {
            r#type: BlockchainType::L1,
            ..Default::default()
        })
        .into();

        let signers: Vec<PrivateKeySigner> = KEYS
            .iter()
            .map(|key| PrivateKeySigner::from_str(key).unwrap())
            .filter(|signer| genesis.alloc.contains_key(&eaddr(signer.address())))
            .take(8)
            .collect();
        assert_eq!(signers.len(), 8, "dev genesis funds too few of the fixture keys");

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
            blockchain,
            genesis_hash,
            genesis_timestamp: genesis_header.timestamp,
            chain_id,
            gas_limit: genesis_header.gas_limit,
            signers,
            proposer: Address::repeat_byte(0x77),
            block_value: U256::from(ETH / 2),
            relay_config,
        }
    }

    fn engine_config(&self, min_emission_interval: Duration) -> EngineConfig {
        EngineConfig {
            relay_signer: self.signers[6].clone(),
            max_blocks_per_slot: 64,
            max_orders_per_slot: 1024,
            min_value_increase_wei: U256::ZERO,
            min_emission_interval,
            core: None,
        }
    }

    fn head(&self) -> watch::Receiver<HeadInfo> {
        let (tx, rx) = watch::channel(HeadInfo {
            number: 0,
            hash: self.genesis_hash,
            timestamp: self.genesis_timestamp,
            is_synced: true,
        });
        std::mem::forget(tx);
        rx
    }

    fn slot_start(&self) -> SlotStartV1 {
        SlotStartV1 {
            slot: SLOT,
            parent_hash: b256(self.genesis_hash),
            proposer_fee_recipient: self.proposer,
            parent_beacon_block_root: B256::ZERO,
        }
    }

    fn build_base(&self, user_value: U256) -> (MergeableBlockV1, B256) {
        self.build_base_with_payment(user_value, self.block_value)
    }

    fn build_base_with_payment(
        &self,
        user_value: U256,
        payment_value: U256,
    ) -> (MergeableBlockV1, B256) {
        let builder = &self.signers[0];
        let base_txs = vec![
            signed_transfer(
                &self.signers[1],
                self.chain_id,
                0,
                Address::repeat_byte(0x55),
                user_value,
                100 * GWEI,
                GWEI,
            ),
            signed_transfer(builder, self.chain_id, 0, self.proposer, payment_value, 100 * GWEI, 0),
        ];
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
        let decoded_txs = base_txs
            .iter()
            .map(|bytes| ethrex_common::types::Transaction::decode_canonical(bytes).unwrap())
            .collect();
        let built = self.blockchain.build_payload_with_transactions(template, decoded_txs).unwrap();
        let payload = block_to_payload_v3(&built.payload);
        let block_hash = payload.payload_inner.payload_inner.block_hash;

        let msg = MergeableBlockV1 {
            slot: SLOT,
            builder_pubkey: Default::default(),
            block_value: payment_value,
            builder_address: builder.address(),
            proposer_fee_recipient: self.proposer,
            parent_beacon_block_root: B256::ZERO,
            allow_appending: true,
            merge_orders: vec![],
            execution_payload: payload,
        };
        (msg, block_hash)
    }

    fn mergeable_tx(
        &self,
        template: &MergeableBlockV1,
        order_sender_ix: usize,
        order_value: U256,
        hash_byte: u8,
    ) -> MergeableBlockV1 {
        let order_tx = signed_transfer(
            &self.signers[order_sender_ix],
            self.chain_id,
            0,
            self.signers[0].address(),
            order_value,
            100 * GWEI,
            GWEI,
        );
        let mut payload = template.execution_payload.clone();
        payload.payload_inner.payload_inner.transactions = vec![order_tx.into()];
        payload.payload_inner.payload_inner.block_hash = B256::repeat_byte(hash_byte);
        payload.payload_inner.payload_inner.fee_recipient = self.signers[2].address();

        MergeableBlockV1 {
            slot: SLOT,
            builder_pubkey: Default::default(),
            block_value: U256::from(ETH / 10),
            builder_address: self.signers[2].address(),
            proposer_fee_recipient: self.proposer,
            parent_beacon_block_root: B256::ZERO,
            allow_appending: false,
            merge_orders: vec![MergeOrderRef::Tx(TxOrderRef { index: 0, can_revert: false })],
            execution_payload: payload,
        }
    }

    fn direct_engine(
        &self,
        min_emission_interval: Duration,
    ) -> (MergeEngine, crossbeam_channel::Receiver<EngineOutput>) {
        let (output_tx, output_rx) = crossbeam_channel::bounded(64);
        let engine = MergeEngine {
            config: self.engine_config(min_emission_interval),
            store: self.store.clone(),
            blockchain: self.blockchain.clone(),
            head: self.head(),
            out: output_tx,
            generation: 0,
            relay_config: None,
            slot: None,
        };
        (engine, output_rx)
    }

    fn direct_engine_with_order_cap(
        &self,
        max_orders_per_slot: usize,
    ) -> (MergeEngine, crossbeam_channel::Receiver<EngineOutput>) {
        let (mut engine, output_rx) = self.direct_engine(Duration::ZERO);
        engine.config.max_orders_per_slot = max_orders_per_slot;
        (engine, output_rx)
    }

    fn mergeable_orders(
        &self,
        template: &MergeableBlockV1,
        pubkey: alloy_rpc_types::beacon::BlsPublicKey,
        senders: &[usize],
        flagged: bool,
        hash_byte: u8,
    ) -> (MergeableBlockV1, Vec<B256>) {
        let mut txs = Vec::with_capacity(senders.len());
        let mut order_hashes = Vec::with_capacity(senders.len());
        let mut merge_orders = Vec::with_capacity(senders.len());
        for (i, &sender_ix) in senders.iter().enumerate() {
            let tx = signed_transfer(
                &self.signers[sender_ix],
                self.chain_id,
                0,
                self.signers[0].address(),
                U256::from(ETH / 5),
                100 * GWEI,
                GWEI,
            );
            let tx_hash = alloy_primitives::keccak256(&tx);
            order_hashes.push(if flagged {
                helix_tcp_types::merging::order::bundle_order_hash(&[tx_hash])
            } else {
                tx_hash
            });
            merge_orders.push(if flagged {
                MergeOrderRef::Bundle(helix_tcp_types::merging::order::BundleOrderRef {
                    txs: vec![i as u16],
                    reverting_txs: vec![],
                    dropping_txs: vec![],
                    latest_only: true,
                })
            } else {
                MergeOrderRef::Tx(TxOrderRef { index: i as u16, can_revert: false })
            });
            txs.push(tx.into());
        }

        let mut payload = template.execution_payload.clone();
        payload.payload_inner.payload_inner.transactions = txs;
        payload.payload_inner.payload_inner.block_hash = B256::repeat_byte(hash_byte);
        payload.payload_inner.payload_inner.fee_recipient = self.signers[2].address();

        let msg = MergeableBlockV1 {
            slot: SLOT,
            builder_pubkey: pubkey,
            block_value: U256::from(ETH / 10),
            builder_address: self.signers[2].address(),
            proposer_fee_recipient: self.proposer,
            parent_beacon_block_root: B256::ZERO,
            allow_appending: false,
            merge_orders,
            execution_payload: payload,
        };
        (msg, order_hashes)
    }

    fn mergeable_bundle(
        &self,
        template: &MergeableBlockV1,
        pubkey: alloy_rpc_types::beacon::BlsPublicKey,
        senders: &[usize],
        hash_byte: u8,
    ) -> (MergeableBlockV1, B256) {
        let mut txs = Vec::with_capacity(senders.len());
        let mut tx_hashes = Vec::with_capacity(senders.len());
        for &sender_ix in senders {
            let tx = signed_transfer(
                &self.signers[sender_ix],
                self.chain_id,
                0,
                self.signers[0].address(),
                U256::from(ETH / 5),
                100 * GWEI,
                GWEI,
            );
            tx_hashes.push(alloy_primitives::keccak256(&tx));
            txs.push(tx.into());
        }
        let mut payload = template.execution_payload.clone();
        payload.payload_inner.payload_inner.transactions = txs;
        payload.payload_inner.payload_inner.block_hash = B256::repeat_byte(hash_byte);
        payload.payload_inner.payload_inner.fee_recipient = self.signers[2].address();

        let msg = MergeableBlockV1 {
            slot: SLOT,
            builder_pubkey: pubkey,
            block_value: U256::from(ETH / 10),
            builder_address: self.signers[2].address(),
            proposer_fee_recipient: self.proposer,
            parent_beacon_block_root: B256::ZERO,
            allow_appending: false,
            merge_orders: vec![MergeOrderRef::Bundle(
                helix_tcp_types::merging::order::BundleOrderRef {
                    txs: (0..senders.len() as u16).collect(),
                    reverting_txs: vec![],
                    dropping_txs: vec![],
                    latest_only: true,
                },
            )],
            execution_payload: payload,
        };
        let hash = helix_tcp_types::merging::order::bundle_order_hash(&tx_hashes);
        (msg, hash)
    }

    fn started_engine(&self) -> (MergeEngine, crossbeam_channel::Receiver<EngineOutput>) {
        let (mut engine, output_rx) = self.direct_engine(Duration::ZERO);
        engine.handle_event(EngineEvent::RelayConfig(self.relay_config.clone()));
        engine.handle_event(EngineEvent::SlotStart(self.slot_start()));
        (engine, output_rx)
    }
}

fn mergeable_event(msg: &MergeableBlockV1, recv_ns: u64) -> EngineEvent {
    EngineEvent::MergeableBlock { body: msg.as_ssz_bytes(), recv_ns, generation: 0 }
}

fn activate_event(block_hash: B256) -> EngineEvent {
    EngineEvent::ActivateBase { slot: SLOT, block_hash, recv_ns: 0, generation: 0 }
}

fn expect_merged(
    output: EngineOutput,
) -> Box<helix_tcp_types::merging::builder_to_relay::MergedBlockV1> {
    match output {
        EngineOutput::Merged { msg, .. } => msg,
        EngineOutput::Reject { msg, .. } => {
            panic!("engine rejected: {:?} {}", msg.code, String::from_utf8_lossy(&msg.msg))
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn merges_order_into_activated_base_block() {
    let _ = tracing_subscriber::fmt().with_env_filter("debug").try_init();
    let fixture = Fixture::new().await;
    let (base_msg, base_block_hash) = fixture.build_base(U256::from(ETH));
    let order_value = U256::from(ETH / 5);
    let mergeable_msg = fixture.mergeable_tx(&base_msg, 3, order_value, 0xdd);
    let base_tx_count = base_msg.execution_payload.payload_inner.payload_inner.transactions.len();

    let (event_tx, event_rx) = crossbeam_channel::bounded(1024);
    let (output_tx, output_rx) = crossbeam_channel::bounded(64);
    let _engine = MergeEngine::spawn(
        fixture.engine_config(Duration::ZERO),
        fixture.store.clone(),
        fixture.blockchain.clone(),
        fixture.head(),
        event_rx,
        output_tx,
    );

    event_tx.send(EngineEvent::RelayConfig(fixture.relay_config.clone())).unwrap();
    event_tx.send(EngineEvent::SlotStart(fixture.slot_start())).unwrap();
    event_tx.send(mergeable_event(&base_msg, 1)).unwrap();
    event_tx.send(mergeable_event(&mergeable_msg, 2)).unwrap();
    event_tx.send(activate_event(base_block_hash)).unwrap();

    let output = output_rx.recv_timeout(Duration::from_secs(60)).expect("engine produced nothing");
    let merged = expect_merged(output);

    assert_eq!(merged.slot, SLOT);
    assert_eq!(merged.base_block_hash, base_block_hash);
    assert!(
        merged.proposer_value > fixture.block_value,
        "proposer value {} must beat the original {}",
        merged.proposer_value,
        fixture.block_value
    );

    let merged_v1 = &merged.execution_payload.payload_inner.payload_inner;
    assert_eq!(merged_v1.transactions.len(), base_tx_count + 2);
    assert_eq!(merged_v1.block_number, 1);
    assert_eq!(merged_v1.parent_hash, b256(fixture.genesis_hash));
    assert_ne!(merged_v1.state_root, B256::ZERO);
    assert_eq!(merged_v1.fee_recipient, fixture.signers[0].address());

    assert_eq!(merged.included_order_ids.len(), 1);
    assert_eq!(merged.builder_inclusions.len(), 1);
    let inclusion = &merged.builder_inclusions[0];
    assert_eq!(inclusion.origin_coinbase, fixture.signers[2].address());
    assert!(
        inclusion.contribution >= order_value,
        "contribution {} must cover the order transfer {order_value}",
        inclusion.contribution
    );
    assert!(merged.appended_blobs.is_empty());

    assert!(output_rx.recv_timeout(Duration::from_millis(500)).is_err());

    let proposer_added = merged.proposer_value - fixture.block_value;
    assert!(proposer_added > U256::ZERO);
    assert!(proposer_added <= inclusion.contribution / U256::from(4) + U256::from(1));

    let _ = aaddr(eaddr(fixture.proposer)); // keep converters exercised both ways
}

#[tokio::test(flavor = "multi_thread")]
async fn checkpoint_hit_reuses_shared_prefix_on_resubmission() {
    let fixture = Fixture::new().await;
    let (base_a, hash_a) = fixture.build_base_with_payment(U256::from(ETH), fixture.block_value);
    let (base_b, hash_b) = fixture
        .build_base_with_payment(U256::from(ETH), fixture.block_value + U256::from(ETH / 100));
    assert_ne!(hash_a, hash_b);
    assert_eq!(
        base_a.execution_payload.payload_inner.payload_inner.transactions[0],
        base_b.execution_payload.payload_inner.payload_inner.transactions[0],
    );

    let (mut engine, output_rx) = fixture.direct_engine(Duration::ZERO);
    engine.handle_event(EngineEvent::RelayConfig(fixture.relay_config.clone()));
    engine.handle_event(EngineEvent::SlotStart(fixture.slot_start()));
    engine.handle_event(mergeable_event(&base_a, 1));
    engine.handle_event(activate_event(hash_a));
    engine.merge_pass();
    {
        let state = engine.slot.as_ref().unwrap();
        assert_eq!(state.checkpoint_misses, 1, "first activation has no checkpoint to hit");
        assert_eq!(state.checkpoint_hits, 0);
    }

    engine.handle_event(mergeable_event(&base_b, 2));
    engine.handle_event(activate_event(hash_b));
    engine.merge_pass();

    {
        let state = engine.slot.as_ref().unwrap();
        assert_eq!(
            state.checkpoint_hits, 1,
            "resubmission sharing the prefix must hit the checkpoint from base_a"
        );
        assert_eq!(state.session.as_ref().unwrap().base_block_hash, hash_b);
    }

    let mergeable_msg = fixture.mergeable_tx(&base_b, 3, U256::from(ETH / 5), 0xdd);
    engine.handle_event(mergeable_event(&mergeable_msg, 3));
    engine.merge_pass();

    let merged =
        expect_merged(output_rx.try_recv().expect("checkpoint-hit session must still emit"));
    assert_eq!(merged.base_block_hash, hash_b);
    assert!(
        merged.proposer_value > fixture.block_value + U256::from(ETH / 100),
        "proposer value {} must beat base_b's own value",
        merged.proposer_value
    );
}

fn pubkey(byte: u8) -> alloy_rpc_types::beacon::BlsPublicKey {
    alloy_rpc_types::beacon::BlsPublicKey::repeat_byte(byte)
}

#[tokio::test(flavor = "multi_thread")]
async fn exclusion_holds_across_later_sessions() {
    let fixture = Fixture::new().await;
    let (base_a, base_a_hash) = fixture.build_base(U256::from(ETH));
    let (base_b, base_b_hash) = fixture.build_base(U256::from(ETH));
    let a = pubkey(0xaa);
    let (with_order, hashes) = fixture.mergeable_orders(&base_a, a, &[3], true, 0xd1);
    let (without_order, _) = fixture.mergeable_orders(&base_a, a, &[], true, 0xd2);

    let (mut engine, output_rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base_a, 1));
    engine.handle_event(mergeable_event(&base_b, 2));
    engine.handle_event(mergeable_event(&with_order, 3));
    engine.handle_event(mergeable_event(&without_order, 4));

    engine.handle_event(activate_event(base_a_hash));
    engine.merge_pass();
    while output_rx.try_recv().is_ok() {}

    engine.handle_event(activate_event(base_b_hash));
    engine.merge_pass();

    assert!(engine.slot.as_ref().unwrap().is_excluded(&hashes[0]));
    while let Ok(out) = output_rx.try_recv() {
        let merged = expect_merged(out);
        let order_id = helix_tcp_types::merging::order::order_id(hashes[0], &a);
        assert!(
            !merged.included_order_ids.contains(&order_id),
            "an excluded order must not appear in a later session's block"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn exclusion_applies_within_the_live_session() {
    let fixture = Fixture::new().await;
    let (base, base_hash) = fixture.build_base(U256::from(ETH));
    let a = pubkey(0xaa);
    let (with_order, hashes) = fixture.mergeable_orders(&base, a, &[3], true, 0xd1);
    let (without_order, _) = fixture.mergeable_orders(&base, a, &[], true, 0xd2);

    let (mut engine, output_rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base, 1));
    engine.handle_event(mergeable_event(&with_order, 2));
    engine.handle_event(activate_event(base_hash));
    engine.handle_event(mergeable_event(&without_order, 3));
    engine.merge_pass();

    assert!(engine.slot.as_ref().unwrap().is_excluded(&hashes[0]));
    let order_id = helix_tcp_types::merging::order::order_id(hashes[0], &a);
    while let Ok(out) = output_rx.try_recv() {
        assert!(!expect_merged(out).included_order_ids.contains(&order_id));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn exclusion_covers_identical_content_from_another_builder() {
    let fixture = Fixture::new().await;
    let (base, _) = fixture.build_base(U256::from(ETH));
    let (a, b) = (pubkey(0xaa), pubkey(0xbb));
    let (a_with, hashes) = fixture.mergeable_orders(&base, a, &[3], true, 0xd1);
    let (b_with, b_hashes) = fixture.mergeable_orders(&base, b, &[3], true, 0xd2);
    assert_eq!(hashes[0], b_hashes[0], "both builders must send the same content");
    let (a_without, _) = fixture.mergeable_orders(&base, a, &[], true, 0xd3);

    let (mut engine, _rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base, 1));
    engine.handle_event(mergeable_event(&a_with, 2));
    engine.handle_event(mergeable_event(&b_with, 3));
    engine.handle_event(mergeable_event(&a_without, 4));

    let state = engine.slot.as_ref().unwrap();
    assert!(state.is_excluded(&hashes[0]));
    assert!(
        state.orders.iter().filter(|o| o.order_hash == hashes[0]).count() >= 2,
        "both contributors' entries must still be pooled"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_later_block_from_another_builder_does_not_restore() {
    let fixture = Fixture::new().await;
    let (base, _) = fixture.build_base(U256::from(ETH));
    let (a, b) = (pubkey(0xaa), pubkey(0xbb));
    let (a_with, hashes) = fixture.mergeable_orders(&base, a, &[3], true, 0xd1);
    let (a_without, _) = fixture.mergeable_orders(&base, a, &[], true, 0xd2);
    let (b_with, _) = fixture.mergeable_orders(&base, b, &[3], true, 0xd3);

    let (mut engine, _rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base, 1));
    engine.handle_event(mergeable_event(&a_with, 2));
    engine.handle_event(mergeable_event(&a_without, 3));
    engine.handle_event(mergeable_event(&b_with, 4));

    assert!(
        engine.slot.as_ref().unwrap().is_excluded(&hashes[0]),
        "a later block must not lift an exclusion"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn exclusion_applies_to_an_order_that_arrives_later() {
    let fixture = Fixture::new().await;
    let (base, _) = fixture.build_base(U256::from(ETH));
    let (a, b) = (pubkey(0xaa), pubkey(0xbb));
    let (a_with, hashes) = fixture.mergeable_orders(&base, a, &[3], true, 0xd1);
    let (a_without, _) = fixture.mergeable_orders(&base, a, &[], true, 0xd2);
    let (b_with, _) = fixture.mergeable_orders(&base, b, &[3], true, 0xd3);

    let (mut engine, _rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base, 1));
    engine.handle_event(mergeable_event(&a_with, 2));
    engine.handle_event(mergeable_event(&a_without, 3));
    engine.handle_event(mergeable_event(&b_with, 4));

    let state = engine.slot.as_ref().unwrap();
    assert!(state.is_excluded(&hashes[0]));
    assert!(state.orders.iter().any(|o| o.order_hash == hashes[0] && o.builder_pubkey == b));
}

#[tokio::test(flavor = "multi_thread")]
async fn unexcluded_orders_remain_candidates() {
    let fixture = Fixture::new().await;
    let (base, _) = fixture.build_base(U256::from(ETH));
    let a = pubkey(0xaa);
    let (full, hashes) = fixture.mergeable_orders(&base, a, &[3, 4, 5], true, 0xd1);
    let (partial, _) = fixture.mergeable_orders(&base, a, &[3, 5], true, 0xd2);

    let (mut engine, _rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base, 1));
    engine.handle_event(mergeable_event(&full, 2));
    engine.handle_event(mergeable_event(&partial, 3));

    let state = engine.slot.as_ref().unwrap();
    assert!(!state.is_excluded(&hashes[0]), "still sent");
    assert!(state.is_excluded(&hashes[1]), "dropped from the newest block");
    assert!(!state.is_excluded(&hashes[2]), "still sent");
}

#[tokio::test(flavor = "multi_thread")]
async fn unflagged_content_is_never_excluded_by_absence() {
    let fixture = Fixture::new().await;
    let (base, _) = fixture.build_base(U256::from(ETH));
    let a = pubkey(0xaa);
    let (with_order, hashes) = fixture.mergeable_orders(&base, a, &[3], false, 0xd1);
    let (gone, _) = fixture.mergeable_orders(&base, a, &[], false, 0xd2);

    let (mut engine, _rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base, 1));
    engine.handle_event(mergeable_event(&with_order, 2));
    engine.handle_event(mergeable_event(&gone, 3));

    assert!(!engine.slot.as_ref().unwrap().is_excluded(&hashes[0]));
}

#[tokio::test(flavor = "multi_thread")]
async fn silence_is_not_exclusion() {
    let fixture = Fixture::new().await;
    let (base, base_hash) = fixture.build_base(U256::from(ETH));
    let a = pubkey(0xaa);
    let (with_order, hashes) = fixture.mergeable_orders(&base, a, &[3], true, 0xd1);

    let (mut engine, _rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base, 1));
    engine.handle_event(mergeable_event(&with_order, 2));
    engine.handle_event(activate_event(base_hash));
    engine.merge_pass();
    engine.merge_pass();

    assert!(!engine.slot.as_ref().unwrap().is_excluded(&hashes[0]));
}

#[tokio::test(flavor = "multi_thread")]
async fn exclusion_does_not_change_pool_membership() {
    let fixture = Fixture::new().await;
    let (base, _) = fixture.build_base(U256::from(ETH));
    let a = pubkey(0xaa);
    let (with_order, hashes) = fixture.mergeable_orders(&base, a, &[3], true, 0xd1);
    let (without_order, _) = fixture.mergeable_orders(&base, a, &[], true, 0xd2);

    let (mut engine, _rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base, 1));
    engine.handle_event(mergeable_event(&with_order, 2));
    let pooled_before = engine.slot.as_ref().unwrap().orders.len();
    engine.handle_event(mergeable_event(&without_order, 3));
    let state = engine.slot.as_ref().unwrap();

    assert!(state.is_excluded(&hashes[0]));
    assert_eq!(state.orders.len(), pooled_before, "nothing leaves the pool mid-slot");
    assert!(state.orders.iter().any(|o| o.order_hash == hashes[0]));
}

#[tokio::test(flavor = "multi_thread")]
async fn exclusions_do_not_cross_a_slot() {
    let fixture = Fixture::new().await;
    let (base, _) = fixture.build_base(U256::from(ETH));
    let a = pubkey(0xaa);
    let (with_order, hashes) = fixture.mergeable_orders(&base, a, &[3], true, 0xd1);
    let (without_order, _) = fixture.mergeable_orders(&base, a, &[], true, 0xd2);

    let (mut engine, _rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base, 1));
    engine.handle_event(mergeable_event(&with_order, 2));
    engine.handle_event(mergeable_event(&without_order, 3));
    assert!(engine.slot.as_ref().unwrap().is_excluded(&hashes[0]));

    let mut next = fixture.slot_start();
    next.slot = SLOT + 1;
    engine.handle_event(EngineEvent::SlotStart(next));

    let state = engine.slot.as_ref().unwrap();
    assert!(!state.is_excluded(&hashes[0]), "exclusions must not cross a slot");
    assert!(state.latest_only.is_empty(), "latest_only state must not cross a slot either");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_block_of_a_slot_excludes_nothing() {
    let fixture = Fixture::new().await;
    let (base, _) = fixture.build_base(U256::from(ETH));
    let a = pubkey(0xaa);
    let (with_order, hashes) = fixture.mergeable_orders(&base, a, &[3, 4], true, 0xd1);

    let (mut engine, _rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base, 1));
    engine.handle_event(mergeable_event(&with_order, 2));

    let mut next = fixture.slot_start();
    next.slot = SLOT + 1;
    engine.handle_event(EngineEvent::SlotStart(next));

    let mut base2 = base.clone();
    base2.slot = SLOT + 1;
    let (mut first, _) = fixture.mergeable_orders(&base2, a, &[3], true, 0xe1);
    first.slot = SLOT + 1;
    engine.handle_event(mergeable_event(&base2, 3));
    engine.handle_event(mergeable_event(&first, 4));

    let state = engine.slot.as_ref().unwrap();
    assert!(!state.is_excluded(&hashes[0]));
    assert!(!state.is_excluded(&hashes[1]), "the previous slot must not exclude here");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_block_for_another_slot_alters_nothing() {
    let fixture = Fixture::new().await;
    let (base, _) = fixture.build_base(U256::from(ETH));
    let a = pubkey(0xaa);
    let (with_order, _) = fixture.mergeable_orders(&base, a, &[3], true, 0xd1);
    let (mut stale, _) = fixture.mergeable_orders(&base, a, &[], true, 0xd2);
    stale.slot = SLOT + 5;

    let (mut engine, _rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base, 1));
    engine.handle_event(mergeable_event(&with_order, 2));
    let flagged_before = engine.slot.as_ref().unwrap().latest_only.clone();
    engine.handle_event(mergeable_event(&stale, 3));

    assert_eq!(
        engine.slot.as_ref().unwrap().latest_only,
        flagged_before,
        "a block for another slot must not alter this slot's latest_only state"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn exclusion_does_not_disturb_an_applied_bundle() {
    let fixture = Fixture::new().await;
    let (base, base_hash) = fixture.build_base(U256::from(ETH));
    let a = pubkey(0xaa);
    let (with_order, order_hash) = fixture.mergeable_bundle(&base, a, &[3, 4], 0xd1);
    let (without_order, _) = fixture.mergeable_orders(&base, a, &[], true, 0xd2);
    let bundle_txs: Vec<_> =
        with_order.execution_payload.payload_inner.payload_inner.transactions.clone();

    let (mut engine, output_rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base, 1));
    engine.handle_event(mergeable_event(&with_order, 2));
    engine.handle_event(activate_event(base_hash));
    engine.merge_pass();

    let merged = expect_merged(output_rx.try_recv().expect("bundle should merge while still sent"));
    let order_id = helix_tcp_types::merging::order::order_id(order_hash, &a);
    assert!(merged.included_order_ids.contains(&order_id), "bundle applied before exclusion");
    let applied_before = merged.execution_payload.payload_inner.payload_inner.transactions.clone();

    engine.handle_event(mergeable_event(&without_order, 3));

    let session = engine.slot.as_ref().unwrap().session.as_ref().expect("session still live");
    assert!(session.has_applied(&order_id), "an applied order stays applied");
    let positions: Vec<usize> = bundle_txs
        .iter()
        .map(|tx| applied_before.iter().position(|t| t == tx).expect("bundle tx present"))
        .collect();
    assert!(
        positions.windows(2).all(|w| w[1] == w[0] + 1),
        "an applied bundle must stay contiguous and ordered"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn emitted_block_stays_valid_across_an_exclusion() {
    let fixture = Fixture::new().await;
    let (base, base_hash) = fixture.build_base(U256::from(ETH));
    let a = pubkey(0xaa);
    let (with_order, _) = fixture.mergeable_orders(&base, a, &[3, 4], true, 0xd1);
    let (partial, _) = fixture.mergeable_orders(&base, a, &[3], true, 0xd2);

    let (mut engine, output_rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base, 1));
    engine.handle_event(mergeable_event(&with_order, 2));
    engine.handle_event(activate_event(base_hash));
    engine.merge_pass();
    while output_rx.try_recv().is_ok() {}

    engine.handle_event(mergeable_event(&partial, 3));
    engine.merge_pass();

    while let Ok(out) = output_rx.try_recv() {
        let merged = expect_merged(out);
        assert!(
            merged.proposer_value > U256::ZERO,
            "an emission after an exclusion must still pay the proposer"
        );
        assert!(
            !merged.execution_payload.payload_inner.payload_inner.transactions.is_empty(),
            "an emission after an exclusion must still carry a payload"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn exclusion_state_is_independent_of_event_interleaving() {
    use rand::{RngCore, SeedableRng};
    use rand_xorshift::XorShiftRng;

    let fixture = Fixture::new().await;
    let (base, base_hash) = fixture.build_base(U256::from(ETH));
    let (a, b) = (pubkey(0xaa), pubkey(0xbb));
    let (a_full, a_hashes) = fixture.mergeable_orders(&base, a, &[3, 4, 5], true, 0xd1);
    let (a_partial, _) = fixture.mergeable_orders(&base, a, &[3, 5], true, 0xd2);
    let (b_full, _) = fixture.mergeable_orders(&base, b, &[4], true, 0xd3);
    let (a_final, _) = fixture.mergeable_orders(&base, a, &[3], true, 0xd4);
    let blocks = [&base, &a_full, &a_partial, &b_full, &a_final];

    let mut expected: Option<Vec<B256>> = None;
    for seed in 0u64..8 {
        let mut rng = XorShiftRng::seed_from_u64(seed);
        let (mut engine, output_rx) = fixture.started_engine();
        for msg in blocks {
            engine.handle_event(mergeable_event(msg, 1));
            match rng.next_u32() % 3 {
                0 => {
                    engine.handle_event(activate_event(base_hash));
                }
                1 => engine.merge_pass(),
                _ => {}
            }
        }
        while output_rx.try_recv().is_ok() {}

        let mut got: Vec<B256> = engine.slot.as_ref().unwrap().excluded.iter().copied().collect();
        got.sort();
        match &expected {
            None => expected = Some(got),
            Some(first) => {
                assert_eq!(&got, first, "interleaving changed the excluded set (seed {seed})")
            }
        }
    }

    let excluded = expected.expect("at least one run");
    assert!(excluded.contains(&a_hashes[1]), "A dropped its second order");
    assert!(excluded.contains(&a_hashes[2]), "A dropped its third order");
    assert!(!excluded.contains(&a_hashes[0]), "A never dropped its first order");
}

#[tokio::test(flavor = "multi_thread")]
async fn every_pooled_order_is_accounted_for() {
    let fixture = Fixture::new().await;
    let (base, base_hash) = fixture.build_base(U256::from(ETH));
    let a = pubkey(0xaa);
    let (full, _) = fixture.mergeable_orders(&base, a, &[3, 4, 5], true, 0xd1);
    let (partial, _) = fixture.mergeable_orders(&base, a, &[3], true, 0xd2);

    let (mut engine, _rx) = fixture.started_engine();
    engine.handle_event(mergeable_event(&base, 1));
    engine.handle_event(mergeable_event(&full, 2));
    engine.handle_event(mergeable_event(&partial, 3));
    engine.handle_event(activate_event(base_hash));
    engine.merge_pass();

    let state = engine.slot.as_ref().unwrap();
    let excluded_in_pool =
        state.orders.iter().filter(|o| state.is_excluded(&o.order_hash)).count() as u64;
    let session = state.session.as_ref().expect("session live");
    assert_eq!(
        session.stats().orders_excluded_skipped,
        excluded_in_pool,
        "the skip counter must account for exactly the excluded pool entries"
    );
}
