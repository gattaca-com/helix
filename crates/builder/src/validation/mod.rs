pub mod error;
mod merkle;
mod parent_state;
pub mod server;
#[cfg(test)]
mod server_tests;
#[cfg(test)]
pub(crate) mod tests;
mod timed_reads;

use std::{
    sync::{Arc, atomic::AtomicUsize, mpsc},
    time::Instant,
};

use alloy_primitives::B256;
use alloy_rpc_types::{
    beacon::{relay::BidTrace, requests::ExecutionRequestsV4},
    engine::ExecutionPayloadV3,
};
use dashmap::DashSet;
use ethrex_blockchain::{BlockchainType, new_evm, vm::StoreVmDatabase};
use ethrex_common::{
    Address as EAddress, U256 as EU256,
    types::{
        AccountUpdate, BlobsBundle, Block, BlockHeader, CELLS_PER_EXT_BLOB, ELASTICITY_MULTIPLIER,
        Receipt, Transaction, TxKind,
    },
    validation::{
        validate_block_pre_execution, validate_gas_used, validate_receipts_root_and_logs_bloom,
        validate_requests_hash,
    },
};
use ethrex_crypto::NativeCrypto;
use ethrex_storage::Store;
use ethrex_vm::{Evm, EvmError, VmDatabase};
use helix_common::{
    PAYMENT_FORWARDER, PAYMENT_FORWARDER_CODE_HASH, payment::multisend_paid_amount,
    payment_forwarder_recipient, simulator::TxDetail,
};
use tokio::sync::watch;

use crate::{
    engine::{
        convert::{aaddr, au256, b256, eaddr, eu256, h256, payload_v3_to_block},
        simulate::balance_of,
    },
    metrics,
    node::HeadInfo,
    validation::{
        error::ValidationError,
        merkle::{MerklePools, UpdateStream},
        parent_state::ParentStateCache,
        timed_reads::TimedReads,
    },
};

#[derive(Debug)]
pub struct PreparedBlock {
    pub block: Block,
    pub parent_header: BlockHeader,
}

#[derive(Debug)]
pub struct ExecutedBlock {
    pub block: Block,
    pub parent_header: BlockHeader,
    pub receipts: Vec<Receipt>,
    pub account_updates: Vec<AccountUpdate>,
    pub tx_details: Vec<TxDetail>,
}

#[derive(Clone)]
pub struct BlockValidator {
    store: Store,
    head: watch::Receiver<HeadInfo>,
    validation_window: u64,
    disallow: Arc<DashSet<alloy_primitives::Address>>,
    parent_state: Arc<ParentStateCache>,
    merkle_pools: Arc<MerklePools>,
}

impl BlockValidator {
    pub fn new(
        store: Store,
        head: watch::Receiver<HeadInfo>,
        validation_window: u64,
        disallow: Arc<DashSet<alloy_primitives::Address>>,
        merkle_pools: usize,
    ) -> Self {
        let merkle_pools = Arc::new(MerklePools::new(&store, merkle_pools));
        Self {
            store,
            head,
            validation_window,
            disallow,
            parent_state: Arc::default(),
            merkle_pools,
        }
    }

    pub fn prepare(
        &self,
        payload: &ExecutionPayloadV3,
        message: &BidTrace,
        parent_beacon_block_root: B256,
        requests: &ExecutionRequestsV4,
    ) -> Result<PreparedBlock, ValidationError> {
        let t = Instant::now();
        let block = self.to_block(payload, parent_beacon_block_root, requests)?;
        let t = metrics::sim_lap("to_block", t);
        self.validate_message_against_header(&block, message)?;
        let t = metrics::sim_lap("check_trace", t);
        let parent_header = self.parent_header(&block.header)?;
        metrics::sim_lap("parent_header", t);
        Ok(PreparedBlock { block, parent_header })
    }

    pub fn validate(
        &self,
        payload: &ExecutionPayloadV3,
        message: &BidTrace,
        parent_beacon_block_root: B256,
        requests: &ExecutionRequestsV4,
        blobs: &BlobsBundle,
        apply_blacklist: bool,
    ) -> Result<ExecutedBlock, ValidationError> {
        let prepared = self.prepare(payload, message, parent_beacon_block_root, requests)?;
        let t = Instant::now();
        self.validate_blobs_bundle(&prepared.block, blobs)?;
        metrics::sim_lap("blobs", t);
        let executed = self.execute(prepared)?;
        let mut t = Instant::now();
        if apply_blacklist {
            self.ensure_not_blacklisted(&executed, message)?;
            t = metrics::sim_lap("blacklist", t);
        }
        self.ensure_payment(&executed, message)?;
        metrics::sim_lap("payment", t);
        Ok(executed)
    }

    /// Relay-internal merged-block path. A merged block's payment is split
    /// across the base block's own payment tx and the appended distribution tx.
    #[allow(clippy::too_many_arguments)]
    pub fn validate_merged(
        &self,
        payload: &ExecutionPayloadV3,
        message: &BidTrace,
        parent_beacon_block_root: B256,
        requests: &ExecutionRequestsV4,
        blobs: &BlobsBundle,
        apply_blacklist: bool,
        base_payment_tx_index: u64,
    ) -> Result<ExecutedBlock, ValidationError> {
        let prepared = self.prepare(payload, message, parent_beacon_block_root, requests)?;
        let t = Instant::now();
        self.validate_blobs_bundle(&prepared.block, blobs)?;
        metrics::sim_lap("blobs", t);
        let executed = self.execute(prepared)?;
        let mut t = Instant::now();
        if apply_blacklist {
            self.ensure_not_blacklisted(&executed, message)?;
            t = metrics::sim_lap("blacklist", t);
        }
        self.ensure_merged_payment(&executed, message, base_payment_tx_index as usize)?;
        metrics::sim_lap("payment", t);
        Ok(executed)
    }

    /// The submission carries one bundle for the whole block, so the block's
    /// blob hashes must match its commitments in order. ethrex's own
    /// `BlobsBundle::validate` is per transaction and does not fit that shape.
    fn validate_blobs_bundle(
        &self,
        block: &Block,
        blobs: &BlobsBundle,
    ) -> Result<(), ValidationError> {
        let versioned_hashes: Vec<_> = block
            .body
            .transactions
            .iter()
            .filter_map(|tx| match tx {
                Transaction::EIP4844Transaction(tx) => Some(tx.blob_versioned_hashes.clone()),
                _ => None,
            })
            .flatten()
            .collect();

        if versioned_hashes.is_empty() && blobs.is_empty() {
            return Ok(());
        }

        if blobs.blobs.len() != blobs.commitments.len() ||
            blobs.blobs.len() * CELLS_PER_EXT_BLOB != blobs.proofs.len()
        {
            return Err(ValidationError::InvalidBlobsBundle);
        }

        blobs
            .validate_blob_commitment_hashes(&versioned_hashes)
            .map_err(|_| ValidationError::InvalidBlobsBundle)?;

        let valid = ethrex_crypto::kzg::verify_cell_kzg_proof_batch(
            &blobs.blobs,
            &blobs.commitments,
            &blobs.proofs,
        )
        .map_err(|_| ValidationError::InvalidBlobsBundle)?;
        if !valid {
            return Err(ValidationError::InvalidBlobsBundle);
        }

        Ok(())
    }

    /// Rejects a block that interacts with a listed address. Interaction means
    /// effect: a state change, or a transaction addressed to it. Reading an
    /// account is not interaction, which is where this parts company with the
    /// reth simulator.
    fn ensure_not_blacklisted(
        &self,
        executed: &ExecutedBlock,
        message: &BidTrace,
    ) -> Result<(), ValidationError> {
        if self.disallow.is_empty() {
            return Ok(());
        }

        // Neither is guaranteed to change state: a block with no priority fees
        // leaves the coinbase untouched, and an unpaid recipient stays absent.
        for address in [aaddr(executed.block.header.coinbase), message.proposer_fee_recipient] {
            if self.disallow.contains(&address) {
                return Err(ValidationError::Blacklist(address));
            }
        }

        // A call that changes nothing leaves no account update behind.
        for tx in &executed.block.body.transactions {
            if let ethrex_common::types::TxKind::Call(to) = tx.to() {
                let to = aaddr(to);
                if self.disallow.contains(&to) {
                    return Err(ValidationError::Blacklist(to));
                }
            }
        }

        // Senders, created accounts, value recipients and storage writes.
        for update in &executed.account_updates {
            let address = aaddr(update.address);
            if self.disallow.contains(&address) {
                return Err(ValidationError::Blacklist(address));
            }
        }

        Ok(())
    }

    /// Executes against the parent state and checks the header against what
    /// execution produced. Writes nothing to the store.
    pub fn execute(&self, prepared: PreparedBlock) -> Result<ExecutedBlock, ValidationError> {
        let PreparedBlock { block, parent_header } = prepared;
        let chain_config = self.store.get_chain_config();

        let t = Instant::now();
        validate_block_pre_execution(&block, &parent_header, &chain_config, ELASTICITY_MULTIPLIER)
            .map_err(|e| ValidationError::PreExecution(e.to_string()))?;
        let t = metrics::sim_lap("pre_execution", t);

        let (vm_db, parent_reads) = self
            .parent_state
            .get(
                &self.store,
                block.header.parent_hash,
                &parent_header,
                chain_config.fork(block.header.timestamp),
            )
            .map_err(|e| ValidationError::Execution(e.to_string()))?;
        let mut vm = new_evm(&BlockchainType::L1, vm_db)
            .map_err(|e| ValidationError::Execution(e.to_string()))?;
        let reads = TimedReads::wrap(parent_reads);
        vm.db.store = reads.clone();
        metrics::sim_lap("vm_setup", t);

        let merkle_pool = self.merkle_pools.checkout();
        let queue_length = AtomicUsize::new(0);
        let (receipts, tx_details, account_updates) = std::thread::scope(|scope| {
            let (mut stream, merkleizer) = merkle_pool
                .as_ref()
                .map(|pool| {
                    let (tx, rx) = mpsc::channel();
                    let (parent_header, queue_length) = (&parent_header, &queue_length);
                    let merkleizer = scope.spawn(move || {
                        pool.blockchain.merkleize_stream(rx, parent_header, queue_length)
                    });
                    (UpdateStream::new(tx, queue_length), merkleizer)
                })
                .unzip();

            let (receipts, tx_details, gas_used) =
                Self::execute_transactions(&mut vm, &block, stream.as_mut())?;
            let t = Instant::now();
            let requests = vm
                .extract_requests(&receipts, &block.header)
                .map_err(|e| ValidationError::Execution(e.to_string()))?;
            if let Some(withdrawals) = &block.body.withdrawals {
                vm.process_withdrawals(withdrawals)
                    .map_err(|e| ValidationError::Execution(e.to_string()))?;
            }
            let t = metrics::sim_lap("requests_withdrawals", t);

            let serial_updates = match stream.take() {
                Some(mut stream) => {
                    stream.flush(&mut vm)?;
                    None
                }
                None => Some(
                    vm.get_state_transitions()
                        .map_err(|e| ValidationError::Execution(e.to_string()))?,
                ),
            };
            let t = metrics::sim_lap("state_transitions", t);

            validate_gas_used(gas_used, &block.header)
                .map_err(|e| ValidationError::PostExecution(e.to_string()))?;
            validate_receipts_root_and_logs_bloom(&block.header, &receipts, &NativeCrypto)
                .map_err(|e| ValidationError::PostExecution(e.to_string()))?;
            validate_requests_hash(&block.header, &chain_config, &requests)
                .map_err(|e| ValidationError::PostExecution(e.to_string()))?;
            let t = metrics::sim_lap("post_execution", t);

            let (account_updates, state_root) = match (merkleizer, serial_updates) {
                (Some(merkleizer), _) => {
                    let (updates_list, account_updates) = merkleizer
                        .join()
                        .map_err(|_| ValidationError::Execution("merkleizer panicked".into()))?
                        .map_err(|e| ValidationError::Store(e.to_string()))?;
                    (account_updates, updates_list.state_trie_hash)
                }
                (None, Some(account_updates)) => {
                    let state_root = self
                        .store
                        .apply_account_updates_batch(block.header.parent_hash, &account_updates)
                        .map_err(|e| ValidationError::Store(e.to_string()))?
                        .ok_or(ValidationError::MissingParentState)?
                        .state_trie_hash;
                    (account_updates, state_root)
                }
                (None, None) => {
                    return Err(ValidationError::Execution("no account updates collected".into()));
                }
            };
            metrics::sim_lap("state_root", t);
            reads.record();
            metrics::sim_block(block.body.transactions.len(), gas_used);

            if state_root != block.header.state_root {
                return Err(ValidationError::StateRootMismatch {
                    got: b256(block.header.state_root),
                    expected: b256(state_root),
                });
            }

            Ok((receipts, tx_details, account_updates))
        })?;

        Ok(ExecutedBlock { block, parent_header, receipts, account_updates, tx_details })
    }

    /// The transaction loop of ethrex's `execute_block`, reading the coinbase
    /// balance after each transaction. Mirrors the pre-Amsterdam gas accounting
    /// only: the V3 payloads this server decodes predate that fork.
    fn execute_transactions(
        vm: &mut Evm,
        block: &Block,
        mut stream: Option<&mut UpdateStream>,
    ) -> Result<(Vec<Receipt>, Vec<TxDetail>, u64), ValidationError> {
        let execution = |e: EvmError| ValidationError::Execution(e.to_string());
        let header = &block.header;
        let t = Instant::now();
        vm.apply_system_calls(header).map_err(execution)?;
        let t = metrics::sim_lap("system_calls", t);
        let transactions = block
            .body
            .get_transactions_with_sender(&NativeCrypto)
            .map_err(|e| ValidationError::Execution(format!("could not recover senders: {e}")))?;
        let t = metrics::sim_lap("sender_recovery", t);

        let coinbase_balance = |vm: &mut Evm| {
            balance_of(vm, header.coinbase).map_err(|e| ValidationError::Execution(e.to_string()))
        };
        let mut balance = coinbase_balance(vm)?;
        let mut receipts = Vec::with_capacity(transactions.len());
        let mut tx_details = Vec::with_capacity(transactions.len());
        let mut gas_used = 0;
        for (tx, sender) in transactions {
            if tx.gas_limit() > header.gas_limit.saturating_sub(gas_used) {
                return Err(ValidationError::Execution(format!(
                    "gas allowance exceeded: used {gas_used} + tx limit {} > block limit {}",
                    tx.gas_limit(),
                    header.gas_limit
                )));
            }
            let (receipt, _) =
                vm.execute_tx(tx, header, &mut gas_used, sender).map_err(execution)?;
            receipts.push(receipt);

            let after = coinbase_balance(vm)?;
            tx_details.push(TxDetail {
                hash: b256(tx.hash(&NativeCrypto)),
                sender: aaddr(sender),
                nonce: tx.nonce(),
                to: match tx.to() {
                    TxKind::Call(to) => Some(aaddr(to)),
                    TxKind::Create => None,
                },
                builder_payment: au256(after.saturating_sub(balance)),
            });
            balance = after;
            if let Some(stream) = stream.as_deref_mut() {
                stream.after_tx(vm)?;
            }
        }
        metrics::sim_lap("txs", t);
        Ok((receipts, tx_details, gas_used))
    }

    /// The balance delta is the ground truth. It falls short when the proposer
    /// also spends, and then the last transaction must be a payment.
    fn ensure_payment(
        &self,
        executed: &ExecutedBlock,
        message: &BidTrace,
    ) -> Result<(), ValidationError> {
        if self.paid_by_balance(executed, message)? {
            return Ok(());
        }

        let last_ix = executed
            .block
            .body
            .transactions
            .len()
            .checked_sub(1)
            .ok_or(ValidationError::ProposerPayment)?;

        let paid = self.recognized_payment_at(executed, message.proposer_fee_recipient, last_ix)?;
        // The regular path is a single trailing payment for exactly the bid.
        if paid != eu256(message.value) {
            return Err(ValidationError::ProposerPayment);
        }
        Ok(())
    }

    /// Merged counterpart of [`Self::ensure_payment`]. `base_payment_tx_index`
    /// comes from the relay; a wrong one finds no payment and fails closed.
    fn ensure_merged_payment(
        &self,
        executed: &ExecutedBlock,
        message: &BidTrace,
        base_payment_tx_index: usize,
    ) -> Result<(), ValidationError> {
        if self.paid_by_balance(executed, message)? {
            return Ok(());
        }

        let last_ix = executed
            .block
            .body
            .transactions
            .len()
            .checked_sub(1)
            .ok_or(ValidationError::ProposerPayment)?;

        let recipient = message.proposer_fee_recipient;
        let mut total = self.recognized_payment_at(executed, recipient, last_ix)?;
        if base_payment_tx_index != last_ix {
            total += self.recognized_payment_at(executed, recipient, base_payment_tx_index)?;
        }

        if total >= eu256(message.value) {
            return Ok(());
        }
        Err(ValidationError::ProposerPayment)
    }

    /// Withdrawals are consensus-layer income, so they count against the rise
    /// rather than towards it.
    fn paid_by_balance(
        &self,
        executed: &ExecutedBlock,
        message: &BidTrace,
    ) -> Result<bool, ValidationError> {
        let recipient = eaddr(message.proposer_fee_recipient);
        let mut before = self.balance_at_parent(&executed.parent_header, recipient)?;
        let after = executed
            .account_updates
            .iter()
            .find(|update| update.address == recipient)
            .and_then(|update| update.info.as_ref().map(|info| info.balance))
            .unwrap_or(before);

        for withdrawal in executed.block.body.withdrawals.iter().flatten() {
            if withdrawal.address == recipient {
                before += EU256::from(withdrawal.amount) * EU256::from(1_000_000_000u64);
            }
        }

        Ok(after >= before + eu256(message.value))
    }

    fn balance_at_parent(
        &self,
        parent_header: &BlockHeader,
        address: EAddress,
    ) -> Result<EU256, ValidationError> {
        let db = StoreVmDatabase::new(self.store.clone(), parent_header.clone())
            .map_err(|e| ValidationError::Store(e.to_string()))?;
        Ok(db
            .get_account_state(address)
            .map_err(|e| ValidationError::Store(e.to_string()))?
            .map(|account| account.balance)
            .unwrap_or_default())
    }

    /// What the transaction at `ix` pays `recipient`. Zero rather than an error
    /// for anything unrecognised: one bad position must not fail the block.
    fn recognized_payment_at(
        &self,
        executed: &ExecutedBlock,
        recipient: alloy_primitives::Address,
        ix: usize,
    ) -> Result<EU256, ValidationError> {
        let (Some(tx), Some(receipt)) =
            (executed.block.body.transactions.get(ix), executed.receipts.get(ix))
        else {
            return Ok(EU256::zero());
        };
        if !receipt.succeeded {
            return Ok(EU256::zero());
        }

        let to = match tx.to() {
            ethrex_common::types::TxKind::Call(to) => Some(to),
            ethrex_common::types::TxKind::Create => None,
        };
        let paid_directly = to == Some(eaddr(recipient)) && tx.data().is_empty();
        let paid_via_forwarder = to == Some(eaddr(PAYMENT_FORWARDER)) &&
            payment_forwarder_recipient(tx.data()) == Some(recipient) &&
            self.forwarder_is_deployed(&executed.parent_header)?;

        let contributed = if paid_directly || paid_via_forwarder {
            tx.value()
        } else {
            eu256(multisend_paid_amount(tx.data(), recipient))
        };
        if contributed.is_zero() {
            return Ok(EU256::zero());
        }

        // A legacy transaction with no chain id is replayable on another chain.
        if tx.chain_id() != Some(self.store.get_chain_config().chain_id) {
            return Ok(EU256::zero());
        }
        if !tx
            .effective_gas_tip(executed.block.header.base_fee_per_gas)
            .unwrap_or_default()
            .is_zero()
        {
            return Ok(EU256::zero());
        }

        Ok(contributed)
    }

    /// A value call to an address with no code succeeds and keeps the value, so
    /// the forwarder shape only pays where its runtime is present.
    fn forwarder_is_deployed(&self, parent_header: &BlockHeader) -> Result<bool, ValidationError> {
        let db = StoreVmDatabase::new(self.store.clone(), parent_header.clone())
            .map_err(|e| ValidationError::Store(e.to_string()))?;
        Ok(db
            .get_account_state(eaddr(PAYMENT_FORWARDER))
            .map_err(|e| ValidationError::Store(e.to_string()))?
            .is_some_and(|account| account.code_hash == h256(PAYMENT_FORWARDER_CODE_HASH)))
    }

    fn to_block(
        &self,
        payload: &ExecutionPayloadV3,
        parent_beacon_block_root: B256,
        requests: &ExecutionRequestsV4,
    ) -> Result<Block, ValidationError> {
        payload_v3_to_block(payload, parent_beacon_block_root, requests)
    }

    /// The relay serves the trace's fields, so a trace that misdescribes a valid
    /// block is still rejected.
    fn validate_message_against_header(
        &self,
        block: &Block,
        message: &BidTrace,
    ) -> Result<(), ValidationError> {
        let header = &block.header;
        let block_hash = b256(block.hash());
        if block_hash != message.block_hash {
            return Err(ValidationError::BlockHashMismatch {
                got: message.block_hash,
                expected: block_hash,
            });
        }
        if b256(header.parent_hash) != message.parent_hash {
            return Err(ValidationError::ParentHashMismatch {
                got: message.parent_hash,
                expected: b256(header.parent_hash),
            });
        }
        if header.gas_limit != message.gas_limit {
            return Err(ValidationError::GasLimitMismatch {
                got: message.gas_limit,
                expected: header.gas_limit,
            });
        }
        if header.gas_used != message.gas_used {
            return Err(ValidationError::GasUsedMismatch {
                got: message.gas_used,
                expected: header.gas_used,
            });
        }
        Ok(())
    }

    /// A parent past the window is refused: its state may be gone, and that store
    /// error would reach the relay as an unclassifiable failure.
    fn parent_header(&self, header: &BlockHeader) -> Result<BlockHeader, ValidationError> {
        let parent = self
            .store
            .get_block_header_by_hash(header.parent_hash)
            .map_err(|e| ValidationError::Store(e.to_string()))?
            .ok_or(ValidationError::MissingParentBlock)?;

        let head = self.head.borrow().number;
        if head.saturating_sub(parent.number) > self.validation_window {
            return Err(ValidationError::BlockTooOld);
        }
        Ok(parent)
    }
}
