use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::{
    beacon::{relay::BidTrace, requests::ExecutionRequestsV4},
    engine::ExecutionPayloadV3,
};
use dashmap::DashSet;
use ethrex_blockchain::{
    Blockchain, BlockchainOptions, BlockchainType,
    fork_choice::apply_fork_choice,
    payload::{BuildPayloadArgs, create_payload},
};
use ethrex_common::{H256, types::ELASTICITY_MULTIPLIER};
use ethrex_rlp::encode::RLPEncode;
use ethrex_storage::Store;
use helix_common::simulator::BlockSimError;
use tokio::sync::watch;

use crate::{
    engine::convert::{
        Amsterdam, b256, block_to_payload_v3, eaddr, eblobs, h256, payload_v3_to_block,
        requests_to_v4,
    },
    node::HeadInfo,
    testing::{
        ETH, GWEI, blob_bundle, deploy_amsterdam_predeploys, deploy_balance_probe,
        deploy_payment_forwarder, dev_genesis_store_with, funded_signers, signed_blob_transfer,
        signed_transfer, signed_unprotected_transfer,
    },
    validation::{BlockValidator, error::ValidationError},
};

const WINDOW: u64 = 3;

/// The proposal slot every fixture block is built for. Amsterdam puts it in the
/// header, so the bid trace and the header have to agree on it.
const SLOT: u64 = 1;

fn empty_bundle() -> ethrex_common::types::BlobsBundle {
    ethrex_common::types::BlobsBundle::default()
}

pub(crate) struct Built {
    payload: ExecutionPayloadV3,
    requests: ExecutionRequestsV4,
    /// The encoded EIP-7928 list, as a Gloas submission would carry it. `None`
    /// before Amsterdam, which is what every pre-Gloas test wants.
    pub(crate) block_access_list: Option<Vec<u8>>,
    slot: u64,
}

impl Built {
    fn block_hash(&self) -> B256 {
        self.payload.payload_inner.payload_inner.block_hash
    }

    pub(crate) fn amsterdam(&self) -> Option<Amsterdam<'_>> {
        self.block_access_list
            .as_deref()
            .map(|block_access_list| Amsterdam { block_access_list, slot: self.slot })
    }
}

pub(crate) struct Fixture {
    store: Store,
    blockchain: Arc<Blockchain>,
    pub(crate) genesis_hash: H256,
    pub(crate) genesis_timestamp: u64,
    pub(crate) chain_id: u64,
    gas_limit: u64,
    pub(crate) signers: Vec<alloy_signer_local::PrivateKeySigner>,
    pub(crate) proposer: Address,
    head: watch::Sender<HeadInfo>,
    disallow: Arc<DashSet<Address>>,
    is_amsterdam: bool,
}

impl Fixture {
    pub(crate) async fn new() -> Self {
        Self::with_genesis(|_| {}).await
    }

    /// Amsterdam from genesis, so every block this fixture builds carries a
    /// block access list and a slot number.
    pub(crate) async fn amsterdam() -> Self {
        Self::with_genesis(|genesis| {
            genesis.config.amsterdam_time = Some(0);
            deploy_amsterdam_predeploys(genesis);
        })
        .await
    }

    pub(crate) async fn with_forwarder() -> Self {
        Self::with_genesis(deploy_payment_forwarder).await
    }

    async fn with_disallowed(listed: &[Address]) -> Self {
        let fixture = Self::with_genesis(|_| {}).await;
        fixture.disallow(listed)
    }

    async fn with_forwarder_and_disallowed(listed: &[Address]) -> Self {
        let fixture = Self::with_genesis(deploy_payment_forwarder).await;
        fixture.disallow(listed)
    }

    async fn with_probe_and_disallowed(listed: &[Address]) -> Self {
        let fixture = Self::with_genesis(|genesis| deploy_balance_probe(genesis, PROBE)).await;
        fixture.disallow(listed)
    }

    pub(crate) fn disallow(self, listed: &[Address]) -> Self {
        for address in listed {
            self.disallow.insert(*address);
        }
        self
    }

    async fn with_genesis(edit: impl FnOnce(&mut ethrex_common::types::Genesis)) -> Self {
        let (store, genesis) = dev_genesis_store_with(edit).await;
        let signers = funded_signers(&genesis, 4);
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
            proposer: signers[3].address(),
            signers,
            disallow: Arc::new(DashSet::new()),
            is_amsterdam: genesis.config.is_amsterdam_activated(genesis_block.header.timestamp),
        }
    }

    pub(crate) fn validator(&self) -> BlockValidator {
        BlockValidator::new(
            self.store.clone(),
            self.head.subscribe(),
            WINDOW,
            self.disallow.clone(),
        )
    }

    /// Builds a valid block on `parent`, paying `self.proposer` in its last tx.
    pub(crate) fn build_on(&self, parent: H256, timestamp: u64, nonce: u64) -> Built {
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
        self.build_block(parent, timestamp, txs, Vec::new())
    }

    /// The proposer spends, so its whole-block balance delta falls short of the
    /// bid value and the payment must be recognised from a transaction.
    pub(crate) fn proposer_spend(&self, nonce: u64) -> Vec<u8> {
        signed_transfer(
            &self.signers[3],
            self.chain_id,
            nonce,
            Address::repeat_byte(0x55),
            U256::from(GWEI),
            100 * GWEI,
            0,
        )
    }

    pub(crate) fn build_block(
        &self,
        parent: H256,
        timestamp: u64,
        txs: Vec<Vec<u8>>,
        withdrawals: Vec<ethrex_common::types::Withdrawal>,
    ) -> Built {
        let builder = &self.signers[0];
        let args = BuildPayloadArgs {
            parent,
            timestamp,
            fee_recipient: eaddr(builder.address()),
            random: H256::zero(),
            withdrawals: Some(withdrawals),
            beacon_root: Some(H256::zero()),
            slot_number: self.is_amsterdam.then_some(SLOT),
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
            block_access_list: built.block_access_list.as_ref().map(|bal| bal.encode_to_vec()),
            slot: SLOT,
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
                .to_block(&built.payload, B256::ZERO, &built.requests, built.amsterdam())
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

    pub(crate) fn signed_call(
        &self,
        signer: &alloy_signer_local::PrivateKeySigner,
        nonce: u64,
        to: Address,
        value: U256,
        input: Vec<u8>,
    ) -> Vec<u8> {
        use alloy_consensus::SignableTransaction;
        use alloy_signer::SignerSync;
        let tx = alloy_consensus::TxEip1559 {
            chain_id: self.chain_id,
            nonce,
            gas_limit: 100_000,
            max_fee_per_gas: 100 * GWEI,
            max_priority_fee_per_gas: 0,
            to: to.into(),
            value,
            access_list: Default::default(),
            input: input.into(),
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        alloy_eips::eip2718::Encodable2718::encoded_2718(&alloy_consensus::TxEnvelope::from(
            tx.into_signed(signature),
        ))
    }

    /// Deploys empty code, so the created account exists in state.
    fn signed_create(&self, signer: &alloy_signer_local::PrivateKeySigner, nonce: u64) -> Vec<u8> {
        use alloy_consensus::SignableTransaction;
        use alloy_signer::SignerSync;
        let tx = alloy_consensus::TxEip1559 {
            chain_id: self.chain_id,
            nonce,
            gas_limit: 100_000,
            max_fee_per_gas: 100 * GWEI,
            max_priority_fee_per_gas: 0,
            to: alloy_primitives::TxKind::Create,
            value: U256::ZERO,
            access_list: Default::default(),
            input: alloy_primitives::hex!("60006000f3").into(),
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        alloy_eips::eip2718::Encodable2718::encoded_2718(&alloy_consensus::TxEnvelope::from(
            tx.into_signed(signature),
        ))
    }

    pub(crate) fn bid_trace(&self, built: &Built) -> BidTrace {
        let header = &built.payload.payload_inner.payload_inner;
        let block =
            payload_v3_to_block(&built.payload, B256::ZERO, &built.requests, built.amsterdam())
                .expect("the fixture builds a convertible payload");
        BidTrace {
            slot: built.slot,
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
        .prepare(&built.payload, &message, B256::ZERO, &built.requests, built.amsterdam())
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
        .to_block(&built.payload, B256::ZERO, &built.requests, built.amsterdam())
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
        .prepare(&built.payload, &message, B256::ZERO, &built.requests, built.amsterdam())
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
        .prepare(&built.payload, &message, B256::ZERO, &built.requests, built.amsterdam())
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
        .prepare(&built.payload, &message, B256::ZERO, &built.requests, built.amsterdam())
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
        .prepare(&built.payload, &message, B256::ZERO, &built.requests, built.amsterdam())
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
        .prepare(&built.payload, &message, B256::ZERO, &built.requests, built.amsterdam())
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
        .prepare(&built.payload, &message, B256::ZERO, &built.requests, built.amsterdam())
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
    message.block_hash = b256(
        fixture
            .validator()
            .to_block(&payload, B256::ZERO, &built.requests, built.amsterdam())
            .unwrap()
            .hash(),
    );

    let error = fixture
        .validator()
        .prepare(&payload, &message, B256::ZERO, &built.requests, built.amsterdam())
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
        .prepare(&built.payload, &message, B256::ZERO, &built.requests, built.amsterdam())
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
        .prepare(&built.payload, &message, B256::ZERO, &built.requests, built.amsterdam())
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
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
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
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
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
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
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
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
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
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
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
    let tampered = Built { payload: built.payload.clone(), requests, ..built };
    let message = fixture.bid_trace(&tampered);

    let error = fixture
        .validator()
        .validate(
            &tampered.payload,
            &message,
            B256::ZERO,
            &tampered.requests,
            &empty_bundle(),
            false,
            tampered.amsterdam(),
        )
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
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
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
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect_err("an unexecutable transaction must be rejected");

    assert!(matches!(error, ValidationError::Execution(_)), "{error}");
}

/// A block whose transactions raise the proposer's balance by the bid value
/// needs no recognisable payment transaction.
#[tokio::test]
async fn a_payment_by_balance_delta_is_accepted() {
    let fixture = Fixture::new().await;
    let value = U256::from(ETH / 2);
    let txs = vec![
        signed_transfer(
            &fixture.signers[1],
            fixture.chain_id,
            0,
            fixture.proposer,
            value,
            100 * GWEI,
            0,
        ),
        signed_transfer(
            &fixture.signers[2],
            fixture.chain_id,
            0,
            Address::repeat_byte(0x66),
            U256::from(1),
            100 * GWEI,
            0,
        ),
    ];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = value;

    fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect("a balance delta covering the bid must be accepted");
}

#[tokio::test]
async fn a_trailing_direct_transfer_is_accepted() {
    let fixture = Fixture::new().await;
    let value = U256::from(ETH / 2);
    let txs = vec![
        fixture.proposer_spend(0),
        signed_transfer(
            &fixture.signers[0],
            fixture.chain_id,
            0,
            fixture.proposer,
            value,
            100 * GWEI,
            0,
        ),
    ];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = value;

    fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect("a trailing direct transfer must be accepted");
}

#[tokio::test]
async fn an_underpaid_block_is_rejected() {
    let fixture = Fixture::new().await;
    let paid = U256::from(ETH / 2);
    let txs = vec![
        fixture.proposer_spend(0),
        signed_transfer(
            &fixture.signers[0],
            fixture.chain_id,
            0,
            fixture.proposer,
            paid,
            100 * GWEI,
            0,
        ),
    ];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = paid + U256::from(1);

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect_err("paying less than the bid must be rejected");

    assert!(matches!(error, ValidationError::ProposerPayment), "{error}");
}

/// A payment transaction that tips the builder is extracting value the bid did
/// not account for.
#[tokio::test]
async fn a_payment_tx_with_a_priority_fee_is_rejected() {
    let fixture = Fixture::new().await;
    let value = U256::from(ETH / 2);
    let txs = vec![
        fixture.proposer_spend(0),
        signed_transfer(
            &fixture.signers[0],
            fixture.chain_id,
            0,
            fixture.proposer,
            value,
            100 * GWEI,
            GWEI,
        ),
    ];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = value;

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect_err("a tipping payment tx must be rejected");

    assert!(matches!(error, ValidationError::ProposerPayment), "{error}");
}

/// EIP-155 replay protection binds the payment to this chain. A legacy
/// transaction carrying no chain id is replayable, so it is not a payment.
#[tokio::test]
async fn an_unprotected_payment_tx_is_rejected() {
    let fixture = Fixture::new().await;
    let value = U256::from(ETH / 2);
    let txs = vec![
        fixture.proposer_spend(0),
        signed_unprotected_transfer(&fixture.signers[0], 0, fixture.proposer, value, 100 * GWEI),
    ];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = value;

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect_err("an unprotected payment tx must be rejected");

    assert!(matches!(error, ValidationError::ProposerPayment), "{error}");
}

/// A withdrawal is consensus-layer income, not the builder's payment, so it is
/// added to the balance the payment is measured against.
#[tokio::test]
async fn a_withdrawal_does_not_pay_the_bid() {
    let fixture = Fixture::new().await;
    let value = U256::from(ETH / 2);
    let withdrawal = ethrex_common::types::Withdrawal {
        index: 0,
        validator_index: 0,
        address: eaddr(fixture.proposer),
        amount: 500_000_000, // gwei, == ETH / 2
    };
    let built = fixture.build_block(
        fixture.genesis_hash,
        fixture.genesis_timestamp + 12,
        vec![fixture.proposer_spend(0)],
        vec![withdrawal],
    );
    let mut message = fixture.bid_trace(&built);
    message.value = value;

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect_err("a withdrawal must not count as the bid payment");

    assert!(matches!(error, ValidationError::ProposerPayment), "{error}");
}

fn forwarder_calldata(timestamp: u64, recipient: Address) -> Vec<u8> {
    let mut calldata = (timestamp as u32).to_be_bytes().to_vec();
    calldata.extend_from_slice(recipient.as_slice());
    calldata
}

#[tokio::test]
async fn a_payment_through_the_forwarder_is_accepted() {
    let fixture = Fixture::with_forwarder().await;
    let value = U256::from(ETH / 2);
    let timestamp = fixture.genesis_timestamp + 12;
    let txs = vec![
        fixture.proposer_spend(0),
        fixture.signed_call(
            &fixture.signers[0],
            0,
            helix_common::PAYMENT_FORWARDER,
            value,
            forwarder_calldata(timestamp, fixture.proposer),
        ),
    ];
    let built = fixture.build_block(fixture.genesis_hash, timestamp, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = value;

    fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect("a forwarder payment must be accepted where the forwarder is deployed");
}

/// A value call to an address with no code succeeds and keeps the value, so the
/// forwarder shape only counts where the forwarder runtime is actually present.
#[tokio::test]
async fn a_forwarder_payment_is_rejected_where_the_forwarder_is_absent() {
    let fixture = Fixture::new().await;
    let value = U256::from(ETH / 2);
    let timestamp = fixture.genesis_timestamp + 12;
    let txs = vec![
        fixture.proposer_spend(0),
        fixture.signed_call(
            &fixture.signers[0],
            0,
            helix_common::PAYMENT_FORWARDER,
            value,
            forwarder_calldata(timestamp, fixture.proposer),
        ),
    ];
    let built = fixture.build_block(fixture.genesis_hash, timestamp, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = value;

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect_err("an undeployed forwarder must not be trusted");

    assert!(matches!(error, ValidationError::ProposerPayment), "{error}");
}

/// The forwarder reverts unless the calldata timestamp matches the block, and a
/// reverted payment pays nothing.
#[tokio::test]
async fn a_reverted_payment_tx_is_rejected() {
    let fixture = Fixture::with_forwarder().await;
    let value = U256::from(ETH / 2);
    let timestamp = fixture.genesis_timestamp + 12;
    let txs = vec![
        fixture.proposer_spend(0),
        fixture.signed_call(
            &fixture.signers[0],
            0,
            helix_common::PAYMENT_FORWARDER,
            value,
            forwarder_calldata(timestamp + 1, fixture.proposer),
        ),
    ];
    let built = fixture.build_block(fixture.genesis_hash, timestamp, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = value;

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect_err("a reverted payment must be rejected");

    assert!(matches!(error, ValidationError::ProposerPayment), "{error}");
}

/// A merged block pays in two places: the base block's own payment transaction,
/// kept at `base_payment_tx_index`, and the distribution transaction appended
/// last.
#[tokio::test]
async fn a_merged_payment_split_across_two_txs_is_accepted() {
    let fixture = Fixture::new().await;
    let base = U256::from(ETH / 4);
    let added = U256::from(ETH / 4);
    let txs = vec![
        fixture.proposer_spend(0),
        signed_transfer(
            &fixture.signers[0],
            fixture.chain_id,
            0,
            fixture.proposer,
            base,
            100 * GWEI,
            0,
        ),
        signed_transfer(
            &fixture.signers[1],
            fixture.chain_id,
            0,
            Address::repeat_byte(0x66),
            U256::from(1),
            100 * GWEI,
            0,
        ),
        signed_transfer(
            &fixture.signers[2],
            fixture.chain_id,
            0,
            fixture.proposer,
            added,
            100 * GWEI,
            0,
        ),
    ];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = base + added;

    fixture
        .validator()
        .validate_merged(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            1,
            built.amsterdam(),
        )
        .expect("both payment positions must count");
}

/// The relay supplies `base_payment_tx_index`. A wrong one must fail closed
/// rather than let an underpaid block through.
#[tokio::test]
async fn a_merged_payment_with_a_wrong_base_index_is_rejected() {
    let fixture = Fixture::new().await;
    let base = U256::from(ETH / 4);
    let added = U256::from(ETH / 4);
    let txs = vec![
        fixture.proposer_spend(0),
        signed_transfer(
            &fixture.signers[0],
            fixture.chain_id,
            0,
            fixture.proposer,
            base,
            100 * GWEI,
            0,
        ),
        signed_transfer(
            &fixture.signers[1],
            fixture.chain_id,
            0,
            Address::repeat_byte(0x66),
            U256::from(1),
            100 * GWEI,
            0,
        ),
        signed_transfer(
            &fixture.signers[2],
            fixture.chain_id,
            0,
            fixture.proposer,
            added,
            100 * GWEI,
            0,
        ),
    ];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = base + added;

    let error = fixture
        .validator()
        .validate_merged(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            2,
            built.amsterdam(),
        )
        .expect_err("a wrong base payment index must fail closed");

    assert!(matches!(error, ValidationError::ProposerPayment), "{error}");
}

/// The regular path only looks at the last transaction, so the same block that
/// passes as a merged submission fails as a regular one.
#[tokio::test]
async fn a_split_payment_is_not_accepted_on_the_regular_path() {
    let fixture = Fixture::new().await;
    let base = U256::from(ETH / 4);
    let added = U256::from(ETH / 4);
    let txs = vec![
        fixture.proposer_spend(0),
        signed_transfer(
            &fixture.signers[0],
            fixture.chain_id,
            0,
            fixture.proposer,
            base,
            100 * GWEI,
            0,
        ),
        signed_transfer(
            &fixture.signers[2],
            fixture.chain_id,
            0,
            fixture.proposer,
            added,
            100 * GWEI,
            0,
        ),
    ];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = base + added;

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect_err("the regular path must not sum two positions");

    assert!(matches!(error, ValidationError::ProposerPayment), "{error}");
}

const LISTED: Address = Address::repeat_byte(0x9a);
const PROBE: Address = Address::repeat_byte(0x9b);

#[tokio::test]
async fn a_blacklisted_sender_is_rejected() {
    let fixture = Fixture::new().await;
    let sender = fixture.signers[1].address();
    let fixture = fixture.disallow(&[sender]);
    let txs = vec![signed_transfer(
        &fixture.signers[1],
        fixture.chain_id,
        0,
        Address::repeat_byte(0x66),
        U256::from(1),
        100 * GWEI,
        0,
    )];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let message = fixture.bid_trace(&built);

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            true,
            built.amsterdam(),
        )
        .expect_err("a listed sender must be rejected");

    assert!(matches!(error, ValidationError::Blacklist(_)), "{error}");
}

#[tokio::test]
async fn a_blacklisted_recipient_is_rejected() {
    let fixture = Fixture::with_disallowed(&[LISTED]).await;
    let txs = vec![signed_transfer(
        &fixture.signers[1],
        fixture.chain_id,
        0,
        LISTED,
        U256::from(1),
        100 * GWEI,
        0,
    )];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let message = fixture.bid_trace(&built);

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            true,
            built.amsterdam(),
        )
        .expect_err("a listed recipient must be rejected");

    assert!(matches!(error, ValidationError::Blacklist(_)), "{error}");
}

#[tokio::test]
async fn a_blacklisted_coinbase_is_rejected() {
    let fixture = Fixture::new().await;
    let coinbase = fixture.signers[0].address();
    let fixture = fixture.disallow(&[coinbase]);
    let txs = vec![signed_transfer(
        &fixture.signers[1],
        fixture.chain_id,
        0,
        Address::repeat_byte(0x66),
        U256::from(1),
        100 * GWEI,
        0,
    )];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let message = fixture.bid_trace(&built);

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            true,
            built.amsterdam(),
        )
        .expect_err("a listed coinbase must be rejected");

    assert!(matches!(error, ValidationError::Blacklist(_)), "{error}");
}

#[tokio::test]
async fn a_blacklisted_proposer_fee_recipient_is_rejected() {
    let fixture = Fixture::with_disallowed(&[Address::repeat_byte(0x9c)]).await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let mut message = fixture.bid_trace(&built);
    message.proposer_fee_recipient = Address::repeat_byte(0x9c);

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            true,
            built.amsterdam(),
        )
        .expect_err("a listed fee recipient must be rejected");

    assert!(matches!(error, ValidationError::Blacklist(_)), "{error}");
}

/// The listed address is never a transaction's `to`: the forwarder sends it the
/// value internally. Only the state change reveals it.
#[tokio::test]
async fn a_blacklisted_internal_value_target_is_rejected() {
    let fixture = Fixture::with_forwarder_and_disallowed(&[LISTED]).await;
    let timestamp = fixture.genesis_timestamp + 12;
    let txs = vec![fixture.signed_call(
        &fixture.signers[1],
        0,
        helix_common::PAYMENT_FORWARDER,
        U256::from(GWEI),
        forwarder_calldata(timestamp, LISTED),
    )];
    let built = fixture.build_block(fixture.genesis_hash, timestamp, txs, Vec::new());
    let message = fixture.bid_trace(&built);

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            true,
            built.amsterdam(),
        )
        .expect_err("a listed internal value target must be rejected");

    assert!(matches!(error, ValidationError::Blacklist(_)), "{error}");
}

#[tokio::test]
async fn a_blacklisted_created_account_is_rejected() {
    let fixture = Fixture::new().await;
    let created = fixture.signers[1].address().create(0);
    let fixture = fixture.disallow(&[created]);
    let txs = vec![fixture.signed_create(&fixture.signers[1], 0)];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let message = fixture.bid_trace(&built);

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            true,
            built.amsterdam(),
        )
        .expect_err("a listed created account must be rejected");

    assert!(matches!(error, ValidationError::Blacklist(_)), "{error}");
}

/// Reading an account is not interacting with it. The reth simulator rejects
/// this block; this one does not.
#[tokio::test]
async fn an_account_that_is_only_read_is_not_blacklisted() {
    let fixture = Fixture::with_probe_and_disallowed(&[LISTED]).await;
    let mut calldata = [0u8; 32];
    calldata[12..].copy_from_slice(LISTED.as_slice());
    let txs =
        vec![fixture.signed_call(&fixture.signers[1], 0, PROBE, U256::ZERO, calldata.to_vec())];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = U256::ZERO;

    let executed = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            true,
            built.amsterdam(),
        )
        .expect("a read alone must not reject the block");

    assert!(executed.receipts[0].succeeded, "the probe must have run for this to prove anything");
}

#[tokio::test]
async fn a_block_touching_no_listed_account_passes() {
    let fixture = Fixture::with_disallowed(&[LISTED]).await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let message = fixture.bid_trace(&built);

    fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            true,
            built.amsterdam(),
        )
        .expect("a block touching nothing listed must pass");
}

/// Filtering is a per-proposer preference, so a non-filtering proposer's block
/// is validated without it.
#[tokio::test]
async fn a_non_filtering_proposer_bypasses_the_blacklist() {
    let fixture = Fixture::with_disallowed(&[LISTED]).await;
    let txs = vec![signed_transfer(
        &fixture.signers[1],
        fixture.chain_id,
        0,
        LISTED,
        U256::from(1),
        100 * GWEI,
        0,
    )];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = U256::ZERO;

    fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect("apply_blacklist = false must skip the check");
}

/// Proves a blob block builds and validates at all, before the negative cases
/// below rely on that.
#[tokio::test]
async fn a_block_with_a_valid_blobs_bundle_passes() {
    let fixture = Fixture::new().await;
    let bundle = blob_bundle(1);
    let hashes: Vec<B256> = bundle.generate_versioned_hashes().iter().map(|h| b256(*h)).collect();
    let txs = vec![signed_blob_transfer(
        &fixture.signers[1],
        fixture.chain_id,
        0,
        Address::repeat_byte(0x66),
        hashes,
    )];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = U256::ZERO;

    let executed = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &bundle,
            false,
            built.amsterdam(),
        )
        .expect("a valid blobs bundle must pass");

    assert!(
        executed.block.header.blob_gas_used.unwrap_or_default() > 0,
        "the blob tx must be in the block for this to prove anything"
    );
}

/// Builds a one-blob block whose bundle the test then damages.
async fn blob_block(fixture: &Fixture) -> (Built, BidTrace, ethrex_common::types::BlobsBundle) {
    let bundle = blob_bundle(1);
    let hashes: Vec<B256> = bundle.generate_versioned_hashes().iter().map(|h| b256(*h)).collect();
    let txs = vec![signed_blob_transfer(
        &fixture.signers[1],
        fixture.chain_id,
        0,
        Address::repeat_byte(0x66),
        hashes,
    )];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let mut message = fixture.bid_trace(&built);
    message.value = U256::ZERO;
    (built, message, bundle)
}

#[tokio::test]
async fn a_bundle_whose_commitments_do_not_match_the_block_is_rejected() {
    let fixture = Fixture::new().await;
    let (built, message, mut bundle) = blob_block(&fixture).await;
    bundle.commitments[0] = blob_bundle(2).commitments[1];

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &bundle,
            false,
            built.amsterdam(),
        )
        .expect_err("commitments not matching the block's hashes must be rejected");

    assert!(matches!(error, ValidationError::InvalidBlobsBundle), "{error}");
}

#[tokio::test]
async fn a_bundle_with_a_wrong_proof_count_is_rejected() {
    let fixture = Fixture::new().await;
    let (built, message, mut bundle) = blob_block(&fixture).await;
    bundle.proofs.pop();

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &bundle,
            false,
            built.amsterdam(),
        )
        .expect_err("a short proof list must be rejected");

    assert!(matches!(error, ValidationError::InvalidBlobsBundle), "{error}");
}

#[tokio::test]
async fn a_bundle_with_an_invalid_cell_proof_is_rejected() {
    let fixture = Fixture::new().await;
    let (built, message, mut bundle) = blob_block(&fixture).await;
    bundle.proofs[0] = ethrex_common::types::Proof::from([0u8; 48]);

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &bundle,
            false,
            built.amsterdam(),
        )
        .expect_err("a corrupt cell proof must be rejected");

    assert!(matches!(error, ValidationError::InvalidBlobsBundle), "{error}");
}

#[tokio::test]
async fn a_bundle_carrying_more_blobs_than_the_block_is_rejected() {
    let fixture = Fixture::new().await;
    let (built, message, _) = blob_block(&fixture).await;
    let bundle = blob_bundle(2);

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &bundle,
            false,
            built.amsterdam(),
        )
        .expect_err("a bundle with a spare blob must be rejected");

    assert!(matches!(error, ValidationError::InvalidBlobsBundle), "{error}");
}

#[tokio::test]
async fn a_blob_tx_with_an_empty_bundle_is_rejected() {
    let fixture = Fixture::new().await;
    let (built, message, _) = blob_block(&fixture).await;

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect_err("a blob tx without its blobs must be rejected");

    assert!(matches!(error, ValidationError::InvalidBlobsBundle), "{error}");
}

/// Blobs the block never referenced are not free to attach.
#[tokio::test]
async fn a_bundle_for_a_block_with_no_blob_txs_is_rejected() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let message = fixture.bid_trace(&built);

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &blob_bundle(1),
            false,
            built.amsterdam(),
        )
        .expect_err("a bundle for a blobless block must be rejected");

    assert!(matches!(error, ValidationError::InvalidBlobsBundle), "{error}");
}

/// The alloy bundle the wire format carries, mirroring [`blob_bundle`].
pub(crate) fn blob_bundle_v2(count: usize) -> alloy_rpc_types::engine::BlobsBundleV2 {
    let bundle = blob_bundle(count);
    alloy_rpc_types::engine::BlobsBundleV2 {
        blobs: bundle.blobs.iter().map(|blob| alloy_primitives::FixedBytes(*blob)).collect(),
        commitments: bundle
            .commitments
            .iter()
            .map(|c| alloy_primitives::FixedBytes(*c).into())
            .collect(),
        proofs: bundle.proofs.iter().map(|p| alloy_primitives::FixedBytes(*p).into()).collect(),
    }
}

impl Fixture {
    /// A block carrying one blob tx per blob in `bundle`.
    pub(crate) fn blob_block(&self, bundle: &alloy_rpc_types::engine::BlobsBundleV2) -> Built {
        let hashes: Vec<B256> =
            eblobs(bundle).generate_versioned_hashes().iter().map(|h| b256(*h)).collect();
        let txs = vec![signed_blob_transfer(
            &self.signers[1],
            self.chain_id,
            0,
            Address::repeat_byte(0x66),
            hashes,
        )];
        self.build_block(self.genesis_hash, self.genesis_timestamp + 12, txs, Vec::new())
    }

    pub(crate) fn submission(
        &self,
        built: &Built,
    ) -> alloy_rpc_types::beacon::relay::SignedBidSubmissionV5 {
        alloy_rpc_types::beacon::relay::SignedBidSubmissionV5 {
            message: self.bid_trace(built),
            execution_payload: built.payload.clone(),
            blobs_bundle: Default::default(),
            execution_requests: built.requests.clone(),
            signature: Default::default(),
        }
    }

    /// SSZ bytes of the submission in the shape the relay sends when it has no
    /// decoder params: a bare `SignedBidSubmissionV5`.
    pub(crate) fn encode_submission(
        &self,
        submission: alloy_rpc_types::beacon::relay::SignedBidSubmissionV5,
    ) -> Vec<u8> {
        ssz::Encode::as_ssz_bytes(&submission)
    }

    /// The request shape the relay sends for a Gloas submission: the Gloas
    /// wire shape, named by its decoder params, with the list inside.
    pub(crate) fn gloas_ssz_request(
        &self,
        built: &Built,
    ) -> helix_common::simulator::SszValidationRequest {
        let submission: helix_types::SignedBidSubmission =
            self.submission(built).try_into().expect("the fixture builds a convertible submission");
        let bal = built.block_access_list.clone().expect("a Gloas request carries a list");
        let gloas = helix_types::SignedBidSubmissionGloas::join(
            submission,
            helix_types::BlockAccessListBytes(bal.into()),
        );
        helix_common::simulator::SszValidationRequest {
            apply_blacklist: false,
            registered_gas_limit: 0,
            parent_beacon_block_root: B256::ZERO,
            inclusion_list: Default::default(),
            decoder_params: Some(helix_common::decoder::SubmissionDecoderParams::plain(
                helix_types::ForkName::Gloas,
            )),
            signed_bid_submission: ssz::Encode::as_ssz_bytes(&gloas),
        }
    }

    pub(crate) fn ssz_request(
        &self,
        built: &Built,
        pays: bool,
    ) -> helix_common::simulator::SszValidationRequest {
        let mut submission = self.submission(built);
        if !pays {
            submission.message.value = U256::ZERO;
        }
        helix_common::simulator::SszValidationRequest {
            apply_blacklist: false,
            registered_gas_limit: 0,
            parent_beacon_block_root: B256::ZERO,
            inclusion_list: Default::default(),
            decoder_params: None,
            signed_bid_submission: self.encode_submission(submission),
        }
    }
}

/// Proves the fixture really is on Amsterdam: without the fork the tests below
/// would silently pass under Fulu rules.
#[tokio::test]
async fn an_amsterdam_fixture_builds_a_block_with_a_block_access_list() {
    let fixture = Fixture::amsterdam().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);

    let bal = built.block_access_list.as_deref().expect("Amsterdam builds a block access list");
    assert!(!bal.is_empty(), "even an idle block touches accounts");

    let block = payload_v3_to_block(&built.payload, B256::ZERO, &built.requests, built.amsterdam())
        .expect("the fixture builds a convertible payload");
    assert_eq!(block.header.slot_number, Some(SLOT));
    assert_eq!(block.header.block_access_list_hash, Some(ethrex_common::utils::keccak(bal)));
}

#[tokio::test]
async fn an_amsterdam_payload_round_trips_to_the_same_block_hash() {
    let fixture = Fixture::amsterdam().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);

    let block = fixture
        .validator()
        .to_block(&built.payload, B256::ZERO, &built.requests, built.amsterdam())
        .expect("a block the fixture built must convert");

    assert_eq!(b256(block.hash()), built.block_hash());
}

#[tokio::test]
async fn a_valid_amsterdam_block_is_accepted() {
    let fixture = Fixture::amsterdam().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let message = fixture.bid_trace(&built);

    fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect("an Amsterdam block the fixture built must validate");
}

/// The block hash covers the list's hash, so changing a byte breaks the
/// commitment the builder signed.
#[tokio::test]
async fn a_tampered_block_access_list_is_rejected() {
    let fixture = Fixture::amsterdam().await;
    let mut built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let message = fixture.bid_trace(&built);
    built.block_access_list.as_mut().expect("Amsterdam builds one")[0] ^= 0xff;

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect_err("a changed block access list must be rejected");

    assert!(matches!(error, ValidationError::BlockHashMismatch { .. }), "{error}");
}

/// The sharper case: a well-formed list, committed to consistently, that is not
/// the list execution produces. Only re-running the block catches this.
#[tokio::test]
async fn a_block_access_list_that_contradicts_execution_is_rejected() {
    let fixture = Fixture::amsterdam().await;
    let mut built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);

    // Another block's list: valid bytes, wrong block.
    let other = fixture.build_block(
        fixture.genesis_hash,
        fixture.genesis_timestamp + 12,
        vec![signed_transfer(
            &fixture.signers[1],
            fixture.chain_id,
            0,
            Address::repeat_byte(0x77),
            U256::from(GWEI),
            100 * GWEI,
            0,
        )],
        Vec::new(),
    );
    built.block_access_list = other.block_access_list;
    assert!(built.block_access_list.is_some(), "the substitute list must exist");

    // Commit to the substituted list, so the hash checks all pass and only
    // execution can tell the difference.
    let block = payload_v3_to_block(&built.payload, B256::ZERO, &built.requests, built.amsterdam())
        .expect("the substituted list still converts");
    let mut message = fixture.bid_trace(&built);
    message.block_hash = b256(block.hash());

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect_err("a list execution did not produce must be rejected");

    let ValidationError::PostExecution(reason) = &error else {
        panic!("{error}");
    };
    assert!(reason.to_lowercase().contains("access list"), "rejected for another reason: {reason}");
}

#[tokio::test]
async fn an_empty_block_access_list_is_rejected() {
    let fixture = Fixture::amsterdam().await;
    let mut built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let message = fixture.bid_trace(&built);
    built.block_access_list = Some(Vec::new());

    let error = fixture
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            built.amsterdam(),
        )
        .expect_err("an empty block access list must be rejected");

    assert!(matches!(error, ValidationError::EmptyBlockAccessList), "{error}");
}

/// A Gloas submission whose payload predates Amsterdam, and the reverse. The
/// header fields are fork-gated, so ethrex's pre-execution checks catch both
/// without a fork check of our own.
#[tokio::test]
async fn a_fork_and_payload_that_disagree_are_rejected() {
    let amsterdam = Fixture::amsterdam().await;
    let built = amsterdam.build_on(amsterdam.genesis_hash, amsterdam.genesis_timestamp + 12, 0);
    let message = amsterdam.bid_trace(&built);
    let error = amsterdam
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            None,
        )
        .expect_err("an Amsterdam block with no list must be rejected");
    assert!(matches!(error, ValidationError::BlockHashMismatch { .. }), "{error}");

    let fulu = Fixture::new().await;
    let built = fulu.build_on(fulu.genesis_hash, fulu.genesis_timestamp + 12, 0);
    let message = fulu.bid_trace(&built);
    let error = fulu
        .validator()
        .validate(
            &built.payload,
            &message,
            B256::ZERO,
            &built.requests,
            &empty_bundle(),
            false,
            Some(Amsterdam { block_access_list: &[0xc0], slot: SLOT }),
        )
        .expect_err("a Fulu block carrying Amsterdam fields must be rejected");
    assert!(matches!(error, ValidationError::BlockHashMismatch { .. }), "{error}");
}
