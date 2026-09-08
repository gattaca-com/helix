use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::{
    beacon::{relay::BidTrace, requests::ExecutionRequestsV4},
    engine::ExecutionPayloadV3,
};
use ethrex_blockchain::{
    Blockchain, BlockchainOptions, BlockchainType,
    fork_choice::apply_fork_choice,
    payload::{BuildPayloadArgs, create_payload},
};
use ethrex_common::{H256, types::ELASTICITY_MULTIPLIER};
use ethrex_storage::Store;
use helix_common::simulator::BlockSimError;
use tokio::sync::watch;

use crate::{
    engine::convert::{
        b256, block_to_payload_v3, eaddr, h256, payload_v3_to_block, requests_to_v4,
    },
    node::HeadInfo,
    testing::{ETH, GWEI, dev_genesis_store, funded_signers, signed_transfer},
    validation::{BlockValidator, error::ValidationError},
};

const WINDOW: u64 = 3;

struct Built {
    payload: ExecutionPayloadV3,
    requests: ExecutionRequestsV4,
}

impl Built {
    fn block_hash(&self) -> B256 {
        self.payload.payload_inner.payload_inner.block_hash
    }
}

struct Fixture {
    store: Store,
    blockchain: Arc<Blockchain>,
    genesis_hash: H256,
    genesis_timestamp: u64,
    chain_id: u64,
    gas_limit: u64,
    signers: Vec<alloy_signer_local::PrivateKeySigner>,
    proposer: Address,
    head: watch::Sender<HeadInfo>,
}

impl Fixture {
    async fn new() -> Self {
        let (store, genesis) = dev_genesis_store().await;
        let genesis_block = genesis.get_block();
        let blockchain: Arc<Blockchain> = Blockchain::new(store.clone(), BlockchainOptions {
            r#type: BlockchainType::L1,
            ..Default::default()
        })
        .into();

        let (head, _) = watch::channel(HeadInfo {
            number: 0,
            hash: genesis_block.hash(),
            timestamp: genesis_block.header.timestamp,
            is_synced: true,
        });

        Self {
            store,
            blockchain,
            head,
            genesis_hash: genesis_block.hash(),
            genesis_timestamp: genesis_block.header.timestamp,
            chain_id: genesis.config.chain_id,
            gas_limit: genesis_block.header.gas_limit,
            signers: funded_signers(&genesis, 4),
            proposer: Address::repeat_byte(0x77),
        }
    }

    fn validator(&self) -> BlockValidator {
        BlockValidator::new(self.store.clone(), self.head.subscribe(), WINDOW)
    }

    /// Builds a valid block on `parent`, paying `self.proposer` in its last tx.
    fn build_on(&self, parent: H256, timestamp: u64, nonce: u64) -> Built {
        let builder = &self.signers[0];
        let txs = vec![signed_transfer(
            builder,
            self.chain_id,
            nonce,
            self.proposer,
            U256::from(ETH / 2),
            100 * GWEI,
            0,
        )];
        let args = BuildPayloadArgs {
            parent,
            timestamp,
            fee_recipient: eaddr(builder.address()),
            random: H256::zero(),
            withdrawals: Some(Vec::new()),
            beacon_root: Some(H256::zero()),
            slot_number: None,
            version: 3,
            elasticity_multiplier: ELASTICITY_MULTIPLIER,
            gas_ceil: self.gas_limit,
        };
        let template = create_payload(&args, &self.store, Default::default()).unwrap();
        let decoded = txs
            .iter()
            .map(|bytes| ethrex_common::types::Transaction::decode_canonical(bytes).unwrap())
            .collect();
        let built = self.blockchain.build_payload_with_transactions(template, decoded).unwrap();

        Built {
            payload: block_to_payload_v3(&built.payload),
            requests: requests_to_v4(&built.requests).unwrap(),
        }
    }

    /// Appends `count` canonical blocks to the chain and returns the new head.
    async fn extend_canonical(&self, count: usize) -> H256 {
        let mut parent = self.genesis_hash;
        let mut timestamp = self.genesis_timestamp;
        for i in 0..count {
            timestamp += 12;
            let built = self.build_on(parent, timestamp, i as u64);
            let block = self
                .validator()
                .to_block(&built.payload, B256::ZERO, &built.requests)
                .expect("the fixture builds a convertible block");
            parent = block.hash();
            let number = block.header.number;
            let timestamp = block.header.timestamp;
            self.blockchain.add_block(block).unwrap();
            apply_fork_choice(&self.store, parent, parent, parent).await.unwrap();
            self.head.send_replace(HeadInfo { number, hash: parent, timestamp, is_synced: true });
        }
        parent
    }

    fn bid_trace(&self, built: &Built) -> BidTrace {
        let header = &built.payload.payload_inner.payload_inner;
        let block = payload_v3_to_block(&built.payload, B256::ZERO, &built.requests)
            .expect("the fixture builds a convertible payload");
        BidTrace {
            slot: 1,
            parent_hash: header.parent_hash,
            block_hash: b256(block.hash()),
            builder_pubkey: Default::default(),
            proposer_pubkey: Default::default(),
            proposer_fee_recipient: self.proposer,
            gas_limit: header.gas_limit,
            gas_used: header.gas_used,
            value: U256::from(ETH / 2),
        }
    }
}

#[tokio::test]
async fn a_valid_submission_prepares_its_block() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let message = fixture.bid_trace(&built);

    let prepared = fixture
        .validator()
        .prepare(&built.payload, &message, B256::ZERO, &built.requests)
        .expect("a block the fixture built must prepare");

    assert_eq!(b256(prepared.block.hash()), built.block_hash());
    assert_eq!(prepared.parent_header.hash(), fixture.genesis_hash);
}

/// The payload carries a `block_hash`, but the validator recomputes it from the
/// converted header. The two must agree, or the conversion has lost a field.
#[tokio::test]
async fn the_payload_round_trips_to_the_same_block_hash() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);

    let block = fixture
        .validator()
        .to_block(&built.payload, B256::ZERO, &built.requests)
        .expect("a block the fixture built must convert");

    assert_eq!(b256(block.hash()), built.block_hash());
}

#[tokio::test]
async fn a_bid_trace_with_the_wrong_block_hash_is_rejected() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let mut message = fixture.bid_trace(&built);
    message.block_hash = B256::repeat_byte(0xaa);

    let error = fixture
        .validator()
        .prepare(&built.payload, &message, B256::ZERO, &built.requests)
        .expect_err("a mismatched block hash must be rejected");

    assert!(matches!(error, ValidationError::BlockHashMismatch { .. }), "{error}");
}

#[tokio::test]
async fn a_bid_trace_with_the_wrong_parent_hash_is_rejected() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let mut message = fixture.bid_trace(&built);
    message.parent_hash = B256::repeat_byte(0xbb);

    let error = fixture
        .validator()
        .prepare(&built.payload, &message, B256::ZERO, &built.requests)
        .expect_err("a mismatched parent hash must be rejected");

    assert!(matches!(error, ValidationError::ParentHashMismatch { .. }), "{error}");
}

#[tokio::test]
async fn a_bid_trace_with_the_wrong_gas_limit_is_rejected() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let mut message = fixture.bid_trace(&built);
    message.gas_limit += 1;

    let error = fixture
        .validator()
        .prepare(&built.payload, &message, B256::ZERO, &built.requests)
        .expect_err("a mismatched gas limit must be rejected");

    assert!(matches!(error, ValidationError::GasLimitMismatch { .. }), "{error}");
}

#[tokio::test]
async fn a_bid_trace_with_the_wrong_gas_used_is_rejected() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let mut message = fixture.bid_trace(&built);
    message.gas_used += 1;

    let error = fixture
        .validator()
        .prepare(&built.payload, &message, B256::ZERO, &built.requests)
        .expect_err("a mismatched gas used must be rejected");

    assert!(matches!(error, ValidationError::GasUsedMismatch { .. }), "{error}");
}

/// A payload edited after the bid was signed hashes to a different block, so it
/// fails the block hash check rather than any per-field check.
#[tokio::test]
async fn a_tampered_payload_fails_the_block_hash_check() {
    let fixture = Fixture::new().await;
    let mut built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let message = fixture.bid_trace(&built);
    built.payload.payload_inner.payload_inner.gas_used += 1;

    let error = fixture
        .validator()
        .prepare(&built.payload, &message, B256::ZERO, &built.requests)
        .expect_err("an edited payload must be rejected");

    assert!(matches!(error, ValidationError::BlockHashMismatch { .. }), "{error}");
}

#[tokio::test]
async fn an_undecodable_transaction_is_rejected() {
    let fixture = Fixture::new().await;
    let mut built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let message = fixture.bid_trace(&built);
    built.payload.payload_inner.payload_inner.transactions = vec![vec![0xde, 0xad].into()];

    let error = fixture
        .validator()
        .prepare(&built.payload, &message, B256::ZERO, &built.requests)
        .expect_err("an undecodable transaction must be rejected");

    assert!(matches!(error, ValidationError::DecodeTransaction(_)), "{error}");
}

#[tokio::test]
async fn an_unknown_parent_is_rejected() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let mut message = fixture.bid_trace(&built);
    let mut payload = built.payload.clone();
    payload.payload_inner.payload_inner.parent_hash = B256::repeat_byte(0xcc);
    message.parent_hash = B256::repeat_byte(0xcc);
    message.block_hash =
        b256(fixture.validator().to_block(&payload, B256::ZERO, &built.requests).unwrap().hash());

    let error = fixture
        .validator()
        .prepare(&payload, &message, B256::ZERO, &built.requests)
        .expect_err("an unknown parent must be rejected");

    assert!(matches!(error, ValidationError::MissingParentBlock), "{error}");
}

#[tokio::test]
async fn a_parent_inside_the_validation_window_is_accepted() {
    let fixture = Fixture::new().await;
    let head = fixture.extend_canonical(WINDOW as usize).await;
    // extend_canonical spent one nonce per block it appended.
    let built = fixture.build_on(head, fixture.genesis_timestamp + 1200, WINDOW);
    let message = fixture.bid_trace(&built);

    fixture
        .validator()
        .prepare(&built.payload, &message, B256::ZERO, &built.requests)
        .expect("a block built on the head must prepare");
}

/// The chain moves on while a submission is in flight. A parent further back
/// than the window is refused rather than validated against stale state.
#[tokio::test]
async fn a_parent_outside_the_validation_window_is_rejected() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let message = fixture.bid_trace(&built);
    fixture.extend_canonical(WINDOW as usize + 1).await;

    let error = fixture
        .validator()
        .prepare(&built.payload, &message, B256::ZERO, &built.requests)
        .expect_err("a parent outside the window must be rejected");

    assert!(matches!(error, ValidationError::BlockTooOld), "{error}");
}

/// The relay classifies a simulation failure by its message text. These two
/// must keep reaching `is_temporary` and `is_too_old`, or a transient failure
/// starts demoting builders.
#[test]
fn the_relay_classifies_the_parent_errors_it_must_retry() {
    let missing =
        BlockSimError::BlockValidationFailed(ValidationError::MissingParentBlock.to_string());
    assert!(missing.is_temporary(), "{missing}");

    let too_old = BlockSimError::BlockValidationFailed(ValidationError::BlockTooOld.to_string());
    assert!(too_old.is_too_old(), "{too_old}");
    assert!(!too_old.is_demotable(), "{too_old}");
}

fn withdrawal_request() -> ExecutionRequestsV4 {
    ExecutionRequestsV4 {
        withdrawals: vec![alloy_eips::eip7002::WithdrawalRequest {
            source_address: Address::repeat_byte(0x11),
            validator_pubkey: alloy_primitives::FixedBytes::repeat_byte(0x22),
            amount: 1,
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn a_valid_block_passes_execution() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let message = fixture.bid_trace(&built);

    let executed = fixture
        .validator()
        .validate(&built.payload, &message, B256::ZERO, &built.requests)
        .expect("a block the fixture built must validate");

    assert_eq!(executed.receipts.len(), 1);
    assert!(!executed.account_updates.is_empty());
}

/// A simulator must never persist what it validates. The submitted block is not
/// stored, and the chain does not move.
#[tokio::test]
async fn validating_a_block_does_not_store_it() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let message = fixture.bid_trace(&built);

    fixture
        .validator()
        .validate(&built.payload, &message, B256::ZERO, &built.requests)
        .expect("a block the fixture built must validate");

    assert_eq!(fixture.store.get_latest_block_number().await.unwrap(), 0);
    assert!(
        fixture.store.get_block_header_by_hash(h256(message.block_hash)).unwrap().is_none(),
        "the validated block must not be in the store"
    );
}

#[tokio::test]
async fn a_tampered_state_root_is_rejected() {
    let fixture = Fixture::new().await;
    let mut built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    built.payload.payload_inner.payload_inner.state_root = B256::repeat_byte(0xaa);
    let message = fixture.bid_trace(&built);

    let error = fixture
        .validator()
        .validate(&built.payload, &message, B256::ZERO, &built.requests)
        .expect_err("a wrong state root must be rejected");

    assert!(matches!(error, ValidationError::StateRootMismatch { .. }), "{error}");
}

#[tokio::test]
async fn a_tampered_gas_used_is_rejected() {
    let fixture = Fixture::new().await;
    let mut built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    built.payload.payload_inner.payload_inner.gas_used += 1;
    let message = fixture.bid_trace(&built);

    let error = fixture
        .validator()
        .validate(&built.payload, &message, B256::ZERO, &built.requests)
        .expect_err("a wrong gas used must be rejected");

    assert!(matches!(error, ValidationError::PostExecution(_)), "{error}");
}

#[tokio::test]
async fn a_tampered_receipts_root_is_rejected() {
    let fixture = Fixture::new().await;
    let mut built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    built.payload.payload_inner.payload_inner.receipts_root = B256::repeat_byte(0xbb);
    let message = fixture.bid_trace(&built);

    let error = fixture
        .validator()
        .validate(&built.payload, &message, B256::ZERO, &built.requests)
        .expect_err("a wrong receipts root must be rejected");

    assert!(matches!(error, ValidationError::PostExecution(_)), "{error}");
}

/// The requests are submitted alongside the payload and commit into the header,
/// so a bundle execution did not produce must fail.
#[tokio::test]
async fn execution_requests_that_the_block_did_not_produce_are_rejected() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let requests = withdrawal_request();
    let tampered = Built { payload: built.payload.clone(), requests };
    let message = fixture.bid_trace(&tampered);

    let error = fixture
        .validator()
        .validate(&tampered.payload, &message, B256::ZERO, &tampered.requests)
        .expect_err("unproduced requests must be rejected");

    assert!(matches!(error, ValidationError::PostExecution(_)), "{error}");
}

/// Proves the pre-execution checks run: a bad base fee is caught by
/// `validate_block_pre_execution`, before any transaction executes.
#[tokio::test]
async fn a_tampered_base_fee_is_rejected_before_execution() {
    let fixture = Fixture::new().await;
    let mut built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    built.payload.payload_inner.payload_inner.base_fee_per_gas += U256::from(1);
    let message = fixture.bid_trace(&built);

    let error = fixture
        .validator()
        .validate(&built.payload, &message, B256::ZERO, &built.requests)
        .expect_err("a wrong base fee must be rejected");

    assert!(matches!(error, ValidationError::PreExecution(_)), "{error}");
}

#[tokio::test]
async fn a_block_with_an_unexecutable_transaction_is_rejected() {
    let fixture = Fixture::new().await;
    let mut built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let bad_nonce = signed_transfer(
        &fixture.signers[1],
        fixture.chain_id,
        99,
        fixture.proposer,
        U256::from(1),
        100 * GWEI,
        0,
    );
    built.payload.payload_inner.payload_inner.transactions.push(bad_nonce.into());
    let message = fixture.bid_trace(&built);

    let error = fixture
        .validator()
        .validate(&built.payload, &message, B256::ZERO, &built.requests)
        .expect_err("an unexecutable transaction must be rejected");

    assert!(matches!(error, ValidationError::Execution(_)), "{error}");
}
