//! Merge-engine tests on an in-memory ethrex store: the end-to-end flow
//! (spawned worker), session parking, throttled-emission retry, and stats.

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

/// Dev keys from ethrex's `fixtures/keys/private_keys_l1.txt`; the test picks
/// the ones the `LocalDevnet` genesis actually funds.
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

/// Shared in-memory ethrex chain plus the merge participants. Signer roles:
/// 0 = winning builder (base coinbase + payment sender), 1 = base user tx,
/// 2 = donor origin coinbase, 3/7 = order senders, 4 = relay fee recipient,
/// 5 = collateral safe (EOA stand-in), 6 = relay signer.
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
        // Keep the sender alive for the test duration.
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

    /// Builds a valid base block ([user tx, proposer payment]) on genesis;
    /// vary `user_value` to get distinct block hashes.
    fn build_base(&self, user_value: U256) -> (MergeableBlockV1, B256) {
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
            signed_transfer(
                builder,
                self.chain_id,
                0,
                self.proposer,
                self.block_value,
                100 * GWEI,
                0,
            ),
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
            block_value: self.block_value,
            builder_address: builder.address(),
            proposer_fee_recipient: self.proposer,
            parent_beacon_block_root: B256::ZERO,
            allow_appending: true,
            merge_orders: vec![],
            execution_payload: payload,
        };
        (msg, block_hash)
    }

    /// A synthetic (never activated) donor block carrying one order: a
    /// transfer paying the winning builder's coinbase.
    fn donor(
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

    /// A direct-drive engine (no worker thread) for tests that assert on
    /// internal state.
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
async fn merges_donor_order_into_activated_base_block() {
    let _ = tracing_subscriber::fmt().with_env_filter("debug").try_init();
    let fixture = Fixture::new().await;
    let (base_msg, base_block_hash) = fixture.build_base(U256::from(ETH));
    let order_value = U256::from(ETH / 5);
    let donor_msg = fixture.donor(&base_msg, 3, order_value, 0xdd);
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
    event_tx.send(mergeable_event(&donor_msg, 2)).unwrap();
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
    // base txs + the merged order tx + the distribution tx
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
        inclusion.revenue >= order_value,
        "revenue {} must cover the order transfer {order_value}",
        inclusion.revenue
    );
    assert!(merged.appended_blobs.is_empty());

    // No further emission without new orders (nothing improved).
    assert!(output_rx.recv_timeout(Duration::from_millis(500)).is_err());

    // 2500 bps of distributable revenue -> proposer share is at most revenue/4.
    let proposer_added = merged.proposer_value - fixture.block_value;
    assert!(proposer_added > U256::ZERO);
    assert!(proposer_added <= inclusion.revenue / U256::from(4) + U256::from(1));

    let _ = aaddr(eaddr(fixture.proposer)); // keep converters exercised both ways
}

/// A→B→A activation flip resumes the parked session instead of re-replaying,
/// preserving its emission bookkeeping; stats counters reflect the work.
#[tokio::test(flavor = "multi_thread")]
async fn base_flip_back_resumes_parked_session() {
    let fixture = Fixture::new().await;
    let (base_a, hash_a) = fixture.build_base(U256::from(ETH));
    let (base_b, hash_b) = fixture.build_base(U256::from(2 * ETH));
    assert_ne!(hash_a, hash_b);
    let donor_msg = fixture.donor(&base_a, 3, U256::from(ETH / 5), 0xdd);

    let (mut engine, output_rx) = fixture.direct_engine(Duration::ZERO);
    engine.handle_event(EngineEvent::RelayConfig(fixture.relay_config.clone()));
    engine.handle_event(EngineEvent::SlotStart(fixture.slot_start()));
    engine.handle_event(mergeable_event(&base_a, 1));
    engine.handle_event(mergeable_event(&base_b, 2));
    engine.handle_event(mergeable_event(&donor_msg, 3));

    // Activate A: fresh session, order applied, emission.
    engine.handle_event(activate_event(hash_a));
    engine.merge_pass();
    let merged_a = expect_merged(output_rx.try_recv().expect("no emission for base A"));
    assert_eq!(merged_a.base_block_hash, hash_a);

    {
        let state = engine.slot.as_ref().unwrap();
        let session = state.session.as_ref().unwrap();
        assert_eq!(session.base_block_hash, hash_a);
        assert!(state.parked.is_empty());
        assert_eq!(session.stats().orders_applied, 1);
        assert_eq!(session.stats().emissions, 1);
        assert!(session.stats().candidates_screened >= 1);
    }

    // Activate B: A is parked, B builds fresh and emits its own merge.
    engine.handle_event(activate_event(hash_b));
    engine.merge_pass();
    let merged_b = expect_merged(output_rx.try_recv().expect("no emission for base B"));
    assert_eq!(merged_b.base_block_hash, hash_b);
    {
        let state = engine.slot.as_ref().unwrap();
        assert_eq!(state.session.as_ref().unwrap().base_block_hash, hash_b);
        assert_eq!(state.parked.len(), 1);
        assert_eq!(state.parked[0].base_block_hash, hash_a);
    }

    // Flip back to A: the parked session resumes with its bookkeeping intact
    // (a rebuilt session would have best_emitted == 0 and would re-emit).
    engine.handle_event(activate_event(hash_a));
    engine.merge_pass();
    {
        let state = engine.slot.as_ref().unwrap();
        let session = state.session.as_ref().unwrap();
        assert_eq!(session.base_block_hash, hash_a);
        assert_eq!(state.parked.len(), 1);
        assert_eq!(state.parked[0].base_block_hash, hash_b);
        assert_eq!(session.stats().emissions, 1, "resumed session kept its stats");
    }
    // Nothing new to emit for the resumed session: same orders, same value.
    assert!(output_rx.try_recv().is_err(), "resume must not re-emit a non-improving block");
}

/// An improvement blocked by the emission-spacing gate is retried by the
/// worker after the window passes, with no further inbound events.
#[tokio::test(flavor = "multi_thread")]
async fn throttled_emission_is_retried() {
    let fixture = Fixture::new().await;
    let (base_msg, base_block_hash) = fixture.build_base(U256::from(ETH));
    let donor_one = fixture.donor(&base_msg, 3, U256::from(ETH / 5), 0xdd);
    let donor_two = fixture.donor(&base_msg, 7, U256::from(ETH / 4), 0xee);

    let (event_tx, event_rx) = crossbeam_channel::bounded(1024);
    let (output_tx, output_rx) = crossbeam_channel::bounded(64);
    let _engine = MergeEngine::spawn(
        fixture.engine_config(Duration::from_millis(300)),
        fixture.store.clone(),
        fixture.blockchain.clone(),
        fixture.head(),
        event_rx,
        output_tx,
    );

    event_tx.send(EngineEvent::RelayConfig(fixture.relay_config.clone())).unwrap();
    event_tx.send(EngineEvent::SlotStart(fixture.slot_start())).unwrap();
    event_tx.send(mergeable_event(&base_msg, 1)).unwrap();
    event_tx.send(mergeable_event(&donor_one, 2)).unwrap();
    event_tx.send(activate_event(base_block_hash)).unwrap();

    let first =
        expect_merged(output_rx.recv_timeout(Duration::from_secs(60)).expect("no first emission"));

    // The second improvement lands right after the first emission and hits the
    // 300ms spacing gate; only the timeout wake-up can emit it.
    event_tx.send(mergeable_event(&donor_two, 3)).unwrap();
    let second = expect_merged(
        output_rx.recv_timeout(Duration::from_secs(10)).expect("throttled emission never retried"),
    );
    assert!(
        second.proposer_value > first.proposer_value,
        "retried emission {} must improve on {}",
        second.proposer_value,
        first.proposer_value
    );
    assert_eq!(second.included_order_ids.len(), 2);
}
