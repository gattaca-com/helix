//! Differential test: does the prefix-execution cache's fast path (resuming from a cached
//! checkpoint) produce the same execution output as full re-execution from scratch, for the two
//! resubmission patterns it targets — a transaction appended, and the last transaction replaced?
//!
//! Builds a real (temp, on-disk) chain state via reth's production provider/database APIs,
//! genesis-funds one signer, and executes real signed transfer transactions through the actual
//! `ValidationApi::execute_block` path — both with the prefix cache enabled (fast path) and
//! disabled (ground truth) — comparing the resulting receipts, gas used, and post-execution
//! bundle state.

use std::sync::Arc;

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{Address, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use reth_db_common::init::init_genesis;
use reth_ethereum::{
    Block, BlockBody,
    chainspec::{ChainSpec, ChainSpecBuilder, MAINNET},
    consensus::EthBeaconConsensus,
    evm::factory::RethEvmFactory,
    node::{EthereumEngineValidator, EthereumNode},
    primitives::RecoveredBlock,
    provider::{
        db::{init_db, mdbx::DatabaseArguments},
        providers::{BlockchainProvider, ProviderFactory, RocksDBBuilder, StaticFileProvider},
    },
};
use reth_node_builder::NodeTypesWithDBAdapter;
use reth_tasks::Runtime;

use super::*;
use crate::common::{RethConsensus, RethEvmConfig, RethProvider};

/// Header gas limit and base fee used for every test block; generous enough for a handful of
/// 21_000-gas transfers, well above what EIP-1559 requires as a minimum base fee.
const GAS_LIMIT: u64 = 30_000_000;
const BASE_FEE: u64 = 1_000_000_000;

fn build_provider(chain_spec: Arc<ChainSpec>) -> (RethProvider, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = init_db(tmp.path().join("db"), DatabaseArguments::default()).expect("init db");
    let static_files_path = tmp.path().join("static_files");
    std::fs::create_dir_all(&static_files_path).expect("create static_files dir");
    let rocksdb_path = tmp.path().join("rocksdb");

    let factory: ProviderFactory<NodeTypesWithDBAdapter<EthereumNode, _>> = ProviderFactory::new(
        db,
        chain_spec,
        StaticFileProvider::read_write(static_files_path).expect("static file provider"),
        RocksDBBuilder::new(&rocksdb_path)
            .with_default_tables()
            .build()
            .expect("rocksdb provider"),
        Runtime::test(),
    )
    .expect("provider factory");

    init_genesis(&factory).expect("init genesis");

    (BlockchainProvider::new(factory).expect("blockchain provider"), tmp)
}

fn build_validation_api(
    provider: RethProvider,
    chain_spec: Arc<ChainSpec>,
    prefix_cache_enabled: bool,
) -> ValidationApi {
    let consensus: Arc<RethConsensus> = Arc::new(EthBeaconConsensus::new(chain_spec.clone()));
    let evm_config: RethEvmConfig =
        RethEvmConfig::new_with_evm_factory(chain_spec.clone(), RethEvmFactory::default());

    ValidationApi::new(
        provider,
        consensus,
        evm_config,
        ValidationApiConfig {
            blacklist_endpoint: String::new(),
            validation_window: ValidationApiConfig::DEFAULT_VALIDATION_WINDOW,
            prefix_cache_enabled,
        },
        Box::new(Runtime::test()),
        Arc::new(EthereumEngineValidator::new(chain_spec)),
    )
}

/// Signs a plain ETH transfer and recovers it back into the `Recovered<TransactionSigned>` shape
/// `execute_block` expects, reusing the same raw-tx recovery helper production code uses.
fn sign_transfer(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    to: Address,
    value: U256,
) -> Recovered<TransactionSigned> {
    let tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: BASE_FEE as u128,
        max_priority_fee_per_gas: 0,
        to: TxKind::Call(to),
        value,
        access_list: Default::default(),
        input: Default::default(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).expect("sign tx");
    let bytes = TxEnvelope::Eip1559(tx.into_signed(signature)).encoded_2718();
    recover_raw_transaction(&bytes).expect("recover signed tx")
}

fn build_block(
    parent_hash: alloy_primitives::B256,
    txs: Vec<Recovered<TransactionSigned>>,
) -> RecoveredBlock<Block> {
    let mut transactions = Vec::with_capacity(txs.len());
    let mut senders = Vec::with_capacity(txs.len());
    for tx in txs {
        let (signed, sender) = tx.into_parts();
        transactions.push(signed);
        senders.push(sender);
    }

    let header = alloy_consensus::Header {
        parent_hash,
        number: 1,
        gas_limit: GAS_LIMIT,
        base_fee_per_gas: Some(BASE_FEE),
        timestamp: 12,
        ..Default::default()
    };

    RecoveredBlock::new_unhashed(
        Block::new(header, BlockBody { transactions, ommers: Vec::new(), withdrawals: None }),
        senders,
    )
}

/// Runs `execute_block` (prefix cache disabled) as ground truth for `txs` against `parent_hash`.
fn execute_ground_truth(
    provider: RethProvider,
    chain_spec: Arc<ChainSpec>,
    parent_hash: alloy_primitives::B256,
    txs: Vec<Recovered<TransactionSigned>>,
) -> ExecutedBlock {
    let api = build_validation_api(provider, chain_spec, false);
    let block = build_block(parent_hash, txs);
    api.execute_block(&block, false, None, CachedReads::default()).expect("ground truth execution")
}

fn assert_same_output(fast: &ExecutedBlock, slow: &ExecutedBlock, case: &str) {
    assert_eq!(
        fast.output.result.gas_used, slow.output.result.gas_used,
        "{case}: gas_used diverged between fast and slow path"
    );
    assert_eq!(
        fast.output.result.receipts.len(),
        slow.output.result.receipts.len(),
        "{case}: receipt count diverged"
    );
    for (idx, (f, s)) in
        fast.output.result.receipts.iter().zip(slow.output.result.receipts.iter()).enumerate()
    {
        assert_eq!(f.status(), s.status(), "{case}: receipt {idx} status diverged");
        assert_eq!(
            f.cumulative_gas_used(),
            s.cumulative_gas_used(),
            "{case}: receipt {idx} cumulative_gas_used diverged"
        );
    }
    assert_eq!(fast.output.state, slow.output.state, "{case}: post-execution bundle state diverged");
}

#[tokio::test]
async fn prefix_cache_matches_full_execution_for_appended_tx() {
    let signer = PrivateKeySigner::random();
    let signer_address = signer.address();
    let recipient = Address::random();
    let chain_id = MAINNET.chain.id();

    let chain_spec = Arc::new(
        ChainSpecBuilder::default()
            .chain(MAINNET.chain)
            .genesis(Genesis {
                alloc: [(
                    signer_address,
                    GenesisAccount {
                        balance: U256::from(1_000u64) * U256::from(10u128.pow(18)),
                        ..Default::default()
                    },
                )]
                .into(),
                ..MAINNET.genesis.clone()
            })
            .paris_activated()
            .build(),
    );

    let (provider, _tmp) = build_provider(chain_spec.clone());
    let genesis_hash = provider.sealed_header(0).unwrap().unwrap().hash();

    let tx0 = sign_transfer(&signer, chain_id, 0, recipient, U256::from(1u64));
    let tx1 = sign_transfer(&signer, chain_id, 1, recipient, U256::from(2u64));
    let tx2 = sign_transfer(&signer, chain_id, 2, recipient, U256::from(3u64));

    // Fast path: first submission (tx0, tx1) populates the prefix cache; second submission
    // appends tx2 on top of the same two leading transactions and should resume from the "full"
    // checkpoint, executing only tx2.
    let fast_api = build_validation_api(provider.clone(), chain_spec.clone(), true);
    let first_block = build_block(genesis_hash, vec![tx0.clone(), tx1.clone()]);
    fast_api
        .execute_block(&first_block, false, None, CachedReads::default())
        .expect("first submission executes");

    let second_block = build_block(genesis_hash, vec![tx0.clone(), tx1.clone(), tx2.clone()]);
    let fast_result = fast_api
        .execute_block(&second_block, false, None, CachedReads::default())
        .expect("second submission (fast path) executes");

    let slow_result =
        execute_ground_truth(provider, chain_spec, genesis_hash, vec![tx0, tx1, tx2]);

    assert_same_output(&fast_result, &slow_result, "appended tx");
}

#[tokio::test]
async fn prefix_cache_matches_full_execution_for_replaced_last_tx() {
    let signer = PrivateKeySigner::random();
    let signer_address = signer.address();
    let recipient = Address::random();
    let chain_id = MAINNET.chain.id();

    let chain_spec = Arc::new(
        ChainSpecBuilder::default()
            .chain(MAINNET.chain)
            .genesis(Genesis {
                alloc: [(
                    signer_address,
                    GenesisAccount {
                        balance: U256::from(1_000u64) * U256::from(10u128.pow(18)),
                        ..Default::default()
                    },
                )]
                .into(),
                ..MAINNET.genesis.clone()
            })
            .paris_activated()
            .build(),
    );

    let (provider, _tmp) = build_provider(chain_spec.clone());
    let genesis_hash = provider.sealed_header(0).unwrap().unwrap().hash();

    let tx0 = sign_transfer(&signer, chain_id, 0, recipient, U256::from(1u64));
    let tx1 = sign_transfer(&signer, chain_id, 1, recipient, U256::from(2u64));
    // Same nonce as tx1, different value -> a distinct, mutually exclusive last transaction.
    let tx1_replaced = sign_transfer(&signer, chain_id, 1, recipient, U256::from(99u64));

    let fast_api = build_validation_api(provider.clone(), chain_spec.clone(), true);
    let first_block = build_block(genesis_hash, vec![tx0.clone(), tx1]);
    fast_api
        .execute_block(&first_block, false, None, CachedReads::default())
        .expect("first submission executes");

    let second_block = build_block(genesis_hash, vec![tx0.clone(), tx1_replaced.clone()]);
    let fast_result = fast_api
        .execute_block(&second_block, false, None, CachedReads::default())
        .expect("second submission (fast path) executes");

    let slow_result =
        execute_ground_truth(provider, chain_spec, genesis_hash, vec![tx0, tx1_replaced]);

    assert_same_output(&fast_result, &slow_result, "replaced last tx");
}
