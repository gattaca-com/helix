use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use ethrex_blockchain::{Blockchain, BlockchainOptions, BlockchainType};
use ethrex_common::types::Transaction;
use ethrex_storage::Store;
use ethrex_vm::VmDatabase;
use helix_types::{BlsPublicKeyBytes, Withdrawal, Withdrawals};

use super::*;
use crate::testing::{ETH, GWEI, dev_genesis_store, funded_signers, signed_transfer};

const PROPOSER: Address = Address::repeat_byte(0x77);
/// Must clear the payout's own gas cost, ~21000 * base fee.
const SUBSIDY: u128 = ETH / 1000;

struct Fixture {
    store: Store,
    blockchain: Arc<Blockchain>,
    signers: Vec<alloy_signer_local::PrivateKeySigner>,
    parent_hash: B256,
    parent_timestamp: u64,
    parent_gas_limit: u64,
    chain_id: u64,
}

impl Fixture {
    async fn new() -> Self {
        let (store, genesis) = dev_genesis_store().await;
        let genesis_block = genesis.get_block();
        let blockchain = Arc::new(Blockchain::new(store.clone(), BlockchainOptions {
            r#type: BlockchainType::L1,
            ..Default::default()
        }));
        Self {
            signers: funded_signers(&genesis, 8),
            parent_hash: crate::engine::convert::b256(genesis_block.hash()),
            parent_timestamp: genesis_block.header.timestamp,
            parent_gas_limit: genesis_block.header.gas_limit,
            chain_id: genesis.config.chain_id,
            store,
            blockchain,
        }
    }

    /// `signers[0]` is the builder: it holds the coinbase and signs the payout.
    fn builder(&self) -> &alloy_signer_local::PrivateKeySigner {
        &self.signers[0]
    }

    fn config(&self) -> BuildingConfig {
        serde_yaml::from_str(&format!(
            "relay_url: \"http://localhost:4040\"\napi_key: \"key\"\n\
             beacon_url: \"http://localhost:3500\"\nsubsidy_wei: {SUBSIDY}\n"
        ))
        .unwrap()
    }

    fn slot(&self) -> SlotContext {
        SlotContext {
            slot: 1,
            parent_hash: self.parent_hash,
            parent_block_number: 0,
            timestamp: self.parent_timestamp + 12,
            prev_randao: B256::repeat_byte(0xcc),
            withdrawals: Withdrawals::default(),
            parent_beacon_block_root: B256::repeat_byte(0xdd),
            proposer_pubkey: BlsPublicKeyBytes::default(),
            proposer_fee_recipient: PROPOSER,
            registered_gas_limit: self.parent_gas_limit,
        }
    }

    /// Admits a transfer from `signers[index]` to the mempool.
    async fn pool_transfer(&self, index: usize, nonce: u64, tip: u128) {
        let encoded = signed_transfer(
            &self.signers[index],
            self.chain_id,
            nonce,
            Address::repeat_byte(0x55),
            U256::from(GWEI),
            100 * GWEI,
            tip,
        );
        let tx = Transaction::decode_canonical(&encoded).unwrap();
        self.blockchain.add_transaction_to_pool(tx).await.unwrap();
    }

    fn build(&self, slot: &SlotContext, config: &BuildingConfig) -> Result<BuiltBlock, BuildError> {
        build(&self.store, &self.blockchain, slot, config, self.builder(), self.chain_id)
    }

    fn build_default(&self) -> Result<BuiltBlock, BuildError> {
        self.build(&self.slot(), &self.config())
    }

    fn parent_header(&self) -> ethrex_common::types::BlockHeader {
        self.store
            .get_block_header_by_hash(crate::engine::convert::h256(self.parent_hash))
            .unwrap()
            .unwrap()
    }
}

/// The trailing transaction, decoded.
fn payout_tx(built: &BuiltBlock) -> &Transaction {
    built.block.body.transactions.last().expect("a block always ends with the payout")
}

#[tokio::test]
async fn mempool_transactions_are_included() {
    let fixture = Fixture::new().await;
    fixture.pool_transfer(1, 0, GWEI).await;
    fixture.pool_transfer(2, 0, GWEI).await;

    let built = fixture.build_default().unwrap();

    assert_eq!(built.block.body.transactions.len(), 3, "two mempool txs plus the payout");
}

#[tokio::test]
async fn the_block_extends_the_parent() {
    let fixture = Fixture::new().await;
    let slot = fixture.slot();

    let built = fixture.build(&slot, &fixture.config()).unwrap();

    let header = &built.block.header;
    assert_eq!(crate::engine::convert::b256(header.parent_hash), fixture.parent_hash);
    assert_eq!(header.number, 1);
    assert_eq!(header.timestamp, slot.timestamp);
    assert_eq!(crate::engine::convert::b256(header.prev_randao), slot.prev_randao);
    assert_eq!(header.extra_data.as_ref(), b"helix-builder");
    assert_eq!(
        header.coinbase,
        eaddr(fixture.builder().address()),
        "the builder is the coinbase, so tips fund the bid"
    );
}

#[tokio::test]
async fn the_header_gas_limit_follows_the_registered_limit() {
    let fixture = Fixture::new().await;
    let mut slot = fixture.slot();
    // Far below the parent, so the 1/1024 clamp binds.
    slot.registered_gas_limit = 1;

    let built = fixture.build(&slot, &fixture.config()).unwrap();

    let delta = fixture.parent_gas_limit / 1024 - 1;
    assert_eq!(
        built.block.header.gas_limit,
        fixture.parent_gas_limit - delta,
        "ethrex's calc_gas_limit clamps the registered limit against the parent"
    );
}

#[tokio::test]
async fn the_payout_is_the_last_transaction_and_pays_the_bid() {
    let fixture = Fixture::new().await;
    fixture.pool_transfer(1, 0, GWEI).await;

    let built = fixture.build_default().unwrap();

    let payout = payout_tx(&built);
    assert_eq!(payout.to(), ethrex_common::types::TxKind::Call(eaddr(PROPOSER)));
    assert_eq!(payout.value(), eu256(built.value));
    assert!(payout.data().is_empty(), "a plain transfer, which the relay recognises");
    assert_eq!(payout.max_priority_fee(), Some(0), "a tip would be refused");
}

#[tokio::test]
async fn the_bid_equals_tips_plus_subsidy_less_the_payout_gas() {
    let fixture = Fixture::new().await;
    fixture.pool_transfer(1, 0, GWEI).await;
    let config = fixture.config();

    let built = fixture.build_default().unwrap();

    let base_fee = built.block.header.base_fee_per_gas.unwrap();
    let tips = EU256::from(21_000u64) * EU256::from(GWEI);
    let gas_cost = EU256::from(config.payout_gas_reserve) * EU256::from(base_fee);
    let expected = tips + EU256::from(SUBSIDY) - gas_cost;

    assert_eq!(eu256(built.value), expected);
}

#[tokio::test]
async fn the_fee_recipient_balance_rises_by_the_bid() {
    let fixture = Fixture::new().await;
    fixture.pool_transfer(1, 0, GWEI).await;

    let built = fixture.build_default().unwrap();

    // The check the relay's simulator makes in `paid_by_balance`.
    let db =
        ethrex_blockchain::vm::StoreVmDatabase::new(fixture.store.clone(), fixture.parent_header())
            .unwrap();
    let before = db
        .get_account_state(eaddr(PROPOSER))
        .unwrap()
        .map(|account| account.balance)
        .unwrap_or_default();
    let after = built
        .account_updates
        .iter()
        .find(|update| update.address == eaddr(PROPOSER))
        .and_then(|update| update.info.as_ref().map(|info| info.balance))
        .expect("the payout must touch the fee recipient");

    assert_eq!(after, before + eu256(built.value));
    assert!(!built.value.is_zero(), "the relay rejects a zero-value block");
}

#[tokio::test]
async fn the_payout_gas_reserve_survives_a_full_block() {
    let fixture = Fixture::new().await;
    let mut slot = fixture.slot();
    // Room for a handful of transfers, so the fill exhausts the block.
    slot.registered_gas_limit = fixture.parent_gas_limit;
    for index in 1..8 {
        for nonce in 0..40 {
            fixture.pool_transfer(index, nonce, GWEI).await;
        }
    }

    let built = fixture.build(&slot, &fixture.config()).unwrap();

    let payout = payout_tx(&built);
    assert_eq!(
        payout.to(),
        ethrex_common::types::TxKind::Call(eaddr(PROPOSER)),
        "the reserve must outlast the fill",
    );
    assert!(
        built.block.header.gas_used <= built.block.header.gas_limit,
        "the restored reserve must not overrun the limit",
    );
}

#[tokio::test]
async fn an_empty_mempool_still_bids_the_subsidy() {
    let fixture = Fixture::new().await;

    let built = fixture.build_default().unwrap();

    assert_eq!(built.block.body.transactions.len(), 1, "the payout alone");
    assert!(!built.value.is_zero(), "an idle testnet must still produce a bid");
}

#[tokio::test]
async fn a_zero_subsidy_and_no_tips_is_refused() {
    let fixture = Fixture::new().await;
    let mut config = fixture.config();
    config.subsidy_wei = 0;

    let err = fixture.build(&fixture.slot(), &config).expect_err("there is nothing to bid");

    assert!(matches!(err, BuildError::NoPayout), "got: {err}");
}

#[tokio::test]
async fn a_payout_the_builder_cannot_afford_is_refused() {
    let fixture = Fixture::new().await;
    let mut config = fixture.config();
    // Beyond anything the dev genesis funds.
    config.subsidy_wei = u128::MAX;

    let err = fixture.build(&fixture.slot(), &config).expect_err("the builder is not that rich");

    assert!(matches!(err, BuildError::PayoutUnaffordable), "got: {err}");
}

#[tokio::test]
async fn a_payout_to_the_builder_itself_is_refused() {
    let fixture = Fixture::new().await;
    let mut slot = fixture.slot();
    slot.proposer_fee_recipient = fixture.builder().address();

    let err = fixture.build(&slot, &fixture.config()).expect_err("paying ourselves proves nothing");

    assert!(matches!(err, BuildError::PayoutToSelf), "got: {err}");
}

#[tokio::test]
async fn withdrawals_are_applied() {
    let fixture = Fixture::new().await;
    let mut slot = fixture.slot();
    let recipient = Address::repeat_byte(0x66);
    slot.withdrawals = Withdrawals::new(vec![Withdrawal {
        index: 1,
        validator_index: 2,
        address: recipient,
        amount: 32_000_000_000,
    }])
    .unwrap();

    let built = fixture.build(&slot, &fixture.config()).unwrap();

    assert!(built.block.header.withdrawals_root.is_some());
    assert_eq!(built.block.body.withdrawals.as_ref().unwrap().len(), 1);
}

#[tokio::test]
async fn blob_transactions_carry_their_sidecar() {
    let fixture = Fixture::new().await;
    let bundle = crate::testing::blob_bundle(1);
    let hashes: Vec<B256> = bundle
        .generate_versioned_hashes()
        .iter()
        .map(|hash| crate::engine::convert::b256(*hash))
        .collect();
    let encoded = crate::testing::signed_blob_transfer(
        &fixture.signers[1],
        fixture.chain_id,
        0,
        Address::repeat_byte(0x55),
        hashes,
    );
    let Transaction::EIP4844Transaction(blob_tx) = Transaction::decode_canonical(&encoded).unwrap()
    else {
        panic!("expected a blob transaction");
    };
    fixture.blockchain.add_blob_transaction_to_pool(blob_tx, bundle).await.unwrap();

    let built = fixture.build_default().unwrap();

    assert_eq!(built.block.body.transactions.len(), 2, "the blob tx plus the payout");
    assert_eq!(
        built.blobs_bundle.blobs.len(),
        1,
        "the sidecar must come through the mempool, not be rebuilt",
    );
    assert_eq!(
        built.block.header.blob_gas_used,
        Some(u64::from(ethrex_common::constants::GAS_PER_BLOB))
    );
}
