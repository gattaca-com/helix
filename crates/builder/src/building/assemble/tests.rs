use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use ethrex_blockchain::{Blockchain, BlockchainOptions, BlockchainType};
use ethrex_common::types::Transaction;
use ethrex_storage::Store;
use ethrex_vm::VmDatabase;
use helix_types::{BlsPublicKeyBytes, Withdrawal, Withdrawals};

use super::*;
use crate::testing::{
    ETH, GWEI, deploy_amsterdam_predeploys, dev_genesis_store_with, funded_signers, signed_transfer,
};

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
    is_amsterdam: bool,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_genesis(|_| {}).await
    }

    /// Amsterdam from genesis, so every block carries a block access list and a
    /// slot number. The EIP-8282 predeploys are required, not optional.
    async fn amsterdam() -> Self {
        Self::with_genesis(|genesis| {
            genesis.config.amsterdam_time = Some(0);
            deploy_amsterdam_predeploys(genesis);
        })
        .await
    }

    async fn with_genesis(edit: impl FnOnce(&mut ethrex_common::types::Genesis)) -> Self {
        let (store, genesis) = dev_genesis_store_with(edit).await;
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
            is_amsterdam: genesis.config.is_amsterdam_activated(genesis_block.header.timestamp),
            store,
            blockchain,
        }
    }

    /// `signers[0]` is the builder: it holds the coinbase and signs the payout.
    fn builder(&self) -> &alloy_signer_local::PrivateKeySigner {
        &self.signers[0]
    }

    fn config(&self) -> BuildingConfig {
        test_config()
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

    /// A transfer with enough gas to create its recipient under Amsterdam,
    /// which the flat 21000 of `pool_transfer` cannot do.
    async fn pool_transfer_creating(&self, index: usize, to: Address) {
        use alloy_consensus::SignableTransaction;
        use alloy_signer::SignerSync;
        let tx = alloy_consensus::TxEip1559 {
            chain_id: self.chain_id,
            nonce: 0,
            gas_limit: 300_000,
            max_fee_per_gas: 100 * GWEI,
            max_priority_fee_per_gas: GWEI,
            to: to.into(),
            value: U256::from(GWEI),
            access_list: Default::default(),
            input: Default::default(),
        };
        let signature = self.signers[index].sign_hash_sync(&tx.signature_hash()).unwrap();
        let encoded = alloy_eips::eip2718::Encodable2718::encoded_2718(
            &alloy_consensus::TxEnvelope::from(tx.into_signed(signature)),
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

    /// A signing context whose spec puts the fixture's fork at genesis, so the
    /// submission shape matches the block shape.
    fn signing(&self) -> helix_common::signing::RelaySigningContext {
        let mut spec = helix_types::ChainSpec::mainnet();
        match self.fork() {
            helix_types::ForkName::Gloas => {
                spec.gloas_fork_epoch = Some(helix_types::Epoch::new(0))
            }
            _ => spec.fulu_fork_epoch = Some(helix_types::Epoch::new(0)),
        }
        helix_common::signing::RelaySigningContext::new(
            helix_types::BlsKeypair::random(),
            Arc::new(helix_common::chain_info::ChainInfo::new(spec, B256::ZERO, 0)),
        )
    }

    fn fork(&self) -> helix_types::ForkName {
        if self.is_amsterdam { helix_types::ForkName::Gloas } else { helix_types::ForkName::Fulu }
    }

    /// The simulation role, over the same store the block was built on.
    fn validator(&self) -> crate::validation::BlockValidator {
        let parent = self.parent_header();
        let (head, _) = tokio::sync::watch::channel(crate::node::HeadInfo {
            number: parent.number,
            hash: parent.hash(),
            timestamp: parent.timestamp,
            is_synced: true,
        });
        crate::validation::BlockValidator::new(
            self.store.clone(),
            head.subscribe(),
            8,
            Arc::new(dashmap::DashSet::new()),
        )
    }

    fn parent_header(&self) -> ethrex_common::types::BlockHeader {
        self.store
            .get_block_header_by_hash(crate::engine::convert::h256(self.parent_hash))
            .unwrap()
            .unwrap()
    }
}

fn test_config() -> BuildingConfig {
    serde_yaml::from_str(&format!(
        "relay_url: \"http://localhost:4040\"\napi_key: \"key\"\n\
         beacon_url: \"http://localhost:3500\"\nsubsidy_wei: {SUBSIDY}\n"
    ))
    .unwrap()
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

// --- Amsterdam ---

#[tokio::test]
async fn an_amsterdam_block_carries_a_block_access_list_and_a_slot_number() {
    let fixture = Fixture::amsterdam().await;

    let built = fixture.build_default().unwrap();

    let bal = built.block_access_list.as_deref().expect("Amsterdam records a list");
    assert!(!bal.is_empty(), "even an idle block touches accounts");
    assert_eq!(
        built.block.header.block_access_list_hash,
        Some(ethrex_common::utils::keccak(bal)),
        "the submission has to carry the bytes the header committed to",
    );
    assert_eq!(built.block.header.slot_number, Some(fixture.slot().slot));
}

/// Setting either field before Amsterdam would change every block hash.
#[tokio::test]
async fn a_pre_amsterdam_block_carries_neither() {
    let fixture = Fixture::new().await;

    let built = fixture.build_default().unwrap();

    assert!(built.block_access_list.is_none());
    assert!(built.block.header.block_access_list_hash.is_none());
    assert!(built.block.header.slot_number.is_none());
}

/// EIP-7843 wants the proposal slot, which is neither the block number nor zero.
#[tokio::test]
async fn the_header_slot_number_is_the_proposal_slot() {
    let fixture = Fixture::amsterdam().await;
    let mut slot = fixture.slot();
    slot.slot = 4_242;

    let built = fixture.build(&slot, &fixture.config()).unwrap();

    assert_eq!(built.block.header.slot_number, Some(4_242));
    assert_ne!(built.block.header.number, 4_242, "not the block number");
}

/// The proposer keeps being paid in-block under Gloas, so the trailing payout
/// is unchanged.
#[tokio::test]
async fn an_amsterdam_block_still_pays_the_proposer_in_block() {
    let fixture = Fixture::amsterdam().await;
    fixture.pool_transfer(1, 0, GWEI).await;

    let built = fixture.build_default().unwrap();

    let payout = payout_tx(&built);
    assert_eq!(payout.to(), ethrex_common::types::TxKind::Call(eaddr(PROPOSER)));
    assert_eq!(payout.value(), eu256(built.value));
    assert!(!built.value.is_zero(), "the relay rejects a zero-value block");
}

/// Amsterdam charges gas for the list's items out of the same block budget the
/// payout reserve is taken from, so the fill can still starve the payout.
#[tokio::test]
async fn an_amsterdam_block_includes_mempool_transactions() {
    let fixture = Fixture::amsterdam().await;
    fixture.pool_transfer(1, 0, GWEI).await;
    fixture.pool_transfer(2, 0, GWEI).await;

    let built = fixture.build_default().unwrap();

    assert_eq!(built.block.body.transactions.len(), 3, "two mempool txs plus the payout");
}

/// A fee recipient's first payment creates its account, and EIP-8037 charges
/// state gas for that. It is the normal case on a fresh testnet: with only the
/// transfer's own 21000 reserved, the payout runs out of gas and every block is
/// lost. Pre-Amsterdam the same payment costs nothing extra.
#[tokio::test]
async fn paying_a_new_account_reserves_the_amsterdam_state_gas() {
    let config = BuildingConfig { payout_gas_reserve: 21_000, ..test_config() };

    assert_eq!(payout_gas_limit(&config, false, false), 21_000, "no such charge before Amsterdam");
    assert_eq!(payout_gas_limit(&config, true, true), 21_000, "an existing account is a transfer");
    assert_eq!(
        payout_gas_limit(&config, true, false),
        204_600,
        "measured against ethrex: 21000 transfer + 120 state bytes at 1530 each",
    );
}

/// The reserve is only useful if the payment it sizes actually succeeds.
#[tokio::test]
async fn a_first_payment_to_a_new_account_succeeds_under_amsterdam() {
    let fixture = Fixture::amsterdam().await;
    let slot = fixture.slot();
    assert!(
        fixture
            .store
            .get_account_info_by_hash(
                crate::engine::convert::h256(fixture.parent_hash),
                eaddr(slot.proposer_fee_recipient),
            )
            .unwrap()
            .is_none(),
        "the fixture's proposer must start absent, or this proves nothing",
    );

    let built = fixture.build(&slot, &fixture.config()).unwrap();

    let payout = payout_tx(&built);
    assert_eq!(payout.gas_limit(), 204_600);
    assert_eq!(payout.value(), eu256(built.value));
}

/// The recipient's account is read from in-block state, so a payment earlier in
/// the same block already counts as creating it.
#[tokio::test]
async fn a_recipient_created_earlier_in_the_block_needs_no_extra_reserve() {
    let fixture = Fixture::amsterdam().await;
    let mut slot = fixture.slot();
    slot.proposer_fee_recipient = Address::repeat_byte(0x55);
    fixture.pool_transfer_creating(1, slot.proposer_fee_recipient).await;

    let built = fixture.build(&slot, &fixture.config()).unwrap();

    assert_eq!(built.block.body.transactions.len(), 2, "the transfer plus the payout");
    let balance = built
        .account_updates
        .iter()
        .find(|update| update.address == eaddr(slot.proposer_fee_recipient))
        .and_then(|update| update.info.as_ref().map(|info| info.balance))
        .expect("both payments must touch the recipient");
    assert_eq!(
        balance,
        eu256(built.value) + ethrex_common::U256::from(GWEI),
        "the earlier transfer has to land, or it created nothing",
    );
    assert_eq!(payout_tx(&built).gas_limit(), 21_000, "already created, so no state charge");
}

/// The strongest check available without a relay: build a block, submit it the
/// way the relay would receive it, and validate it with our own simulation
/// role. A disagreement between steps 3 and 4 shows up here and nowhere else.
async fn our_simulator_accepts_our_own_block(fixture: &Fixture) {
    use axum::http::StatusCode;
    use tower::ServiceExt;

    let slot = fixture.slot();
    let built = fixture.build(&slot, &fixture.config()).unwrap();
    let bid = crate::building::submit::Submitter::new(
        "http://localhost:1",
        "key".to_string(),
        fixture.signing(),
    )
    .sign(&built, &slot)
    .expect("a block we built must be submittable");

    let request = helix_common::simulator::SszValidationRequest {
        apply_blacklist: false,
        registered_gas_limit: slot.registered_gas_limit,
        parent_beacon_block_root: slot.parent_beacon_block_root,
        inclusion_list: Default::default(),
        decoder_params: Some(helix_common::decoder::SubmissionDecoderParams::plain(fixture.fork())),
        signed_bid_submission: bid.as_ssz_bytes(),
    };

    let response = crate::validation::server::router(fixture.validator(), 1)
        .oneshot(
            axum::http::Request::post("/validate")
                .body(axum::body::Body::from(ssz::Encode::as_ssz_bytes(&request)))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();

    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
}

#[tokio::test]
async fn our_simulation_role_accepts_our_gloas_block() {
    our_simulator_accepts_our_own_block(&Fixture::amsterdam().await).await;
}

#[tokio::test]
async fn our_simulation_role_accepts_our_fulu_block() {
    our_simulator_accepts_our_own_block(&Fixture::new().await).await;
}
