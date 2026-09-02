// Reached from main once the servers land in step 9 of #527.
#![allow(dead_code)]

pub mod error;
#[cfg(test)]
mod tests;

use alloy_primitives::B256;
use alloy_rpc_types::{
    beacon::{relay::BidTrace, requests::ExecutionRequestsV4},
    engine::ExecutionPayloadV3,
};
use ethrex_blockchain::{BlockchainType, new_evm, vm::StoreVmDatabase};
use ethrex_common::{
    Address as EAddress, U256 as EU256,
    types::{AccountUpdate, Block, BlockHeader, ELASTICITY_MULTIPLIER, Receipt},
    validation::{
        validate_block_pre_execution, validate_gas_used, validate_receipts_root_and_logs_bloom,
        validate_requests_hash,
    },
};
use ethrex_crypto::NativeCrypto;
use ethrex_storage::Store;
use ethrex_vm::VmDatabase;
use helix_common::{
    PAYMENT_FORWARDER, PAYMENT_FORWARDER_CODE_HASH, payment::multisend_paid_amount,
    payment_forwarder_recipient,
};
use tokio::sync::watch;

use crate::{
    engine::convert::{b256, eaddr, eu256, h256, payload_v3_to_block},
    node::HeadInfo,
    validation::error::ValidationError,
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
}

#[derive(Clone)]
pub struct BlockValidator {
    store: Store,
    head: watch::Receiver<HeadInfo>,
    validation_window: u64,
}

impl BlockValidator {
    pub fn new(store: Store, head: watch::Receiver<HeadInfo>, validation_window: u64) -> Self {
        Self { store, head, validation_window }
    }

    pub fn prepare(
        &self,
        payload: &ExecutionPayloadV3,
        message: &BidTrace,
        parent_beacon_block_root: B256,
        requests: &ExecutionRequestsV4,
    ) -> Result<PreparedBlock, ValidationError> {
        let block = self.to_block(payload, parent_beacon_block_root, requests)?;
        self.validate_message_against_header(&block, message)?;
        let parent_header = self.parent_header(&block.header)?;
        Ok(PreparedBlock { block, parent_header })
    }

    pub fn validate(
        &self,
        payload: &ExecutionPayloadV3,
        message: &BidTrace,
        parent_beacon_block_root: B256,
        requests: &ExecutionRequestsV4,
    ) -> Result<ExecutedBlock, ValidationError> {
        let prepared = self.prepare(payload, message, parent_beacon_block_root, requests)?;
        let executed = self.execute(prepared)?;
        self.ensure_payment(&executed, message)?;
        Ok(executed)
    }

    /// Relay-internal merged-block path. A merged block's payment is split
    /// across the base block's own payment tx and the appended distribution tx.
    pub fn validate_merged(
        &self,
        payload: &ExecutionPayloadV3,
        message: &BidTrace,
        parent_beacon_block_root: B256,
        requests: &ExecutionRequestsV4,
        base_payment_tx_index: u64,
    ) -> Result<ExecutedBlock, ValidationError> {
        let prepared = self.prepare(payload, message, parent_beacon_block_root, requests)?;
        let executed = self.execute(prepared)?;
        self.ensure_merged_payment(&executed, message, base_payment_tx_index as usize)?;
        Ok(executed)
    }

    /// Executes against the parent state and checks the header against what
    /// execution produced. Writes nothing to the store.
    pub fn execute(&self, prepared: PreparedBlock) -> Result<ExecutedBlock, ValidationError> {
        let PreparedBlock { block, parent_header } = prepared;
        let chain_config = self.store.get_chain_config();

        validate_block_pre_execution(&block, &parent_header, &chain_config, ELASTICITY_MULTIPLIER)
            .map_err(|e| ValidationError::PreExecution(e.to_string()))?;

        let parent_header_for_reads = parent_header.clone();
        let vm_db = StoreVmDatabase::new(self.store.clone(), parent_header)
            .map_err(|e| ValidationError::Execution(e.to_string()))?;
        let mut vm = new_evm(&BlockchainType::L1, vm_db)
            .map_err(|e| ValidationError::Execution(e.to_string()))?;

        let (result, _bal) =
            vm.execute_block(&block).map_err(|e| ValidationError::Execution(e.to_string()))?;

        validate_gas_used(result.block_gas_used, &block.header)
            .map_err(|e| ValidationError::PostExecution(e.to_string()))?;
        validate_receipts_root_and_logs_bloom(&block.header, &result.receipts, &NativeCrypto)
            .map_err(|e| ValidationError::PostExecution(e.to_string()))?;
        validate_requests_hash(&block.header, &chain_config, &result.requests)
            .map_err(|e| ValidationError::PostExecution(e.to_string()))?;

        let account_updates =
            vm.get_state_transitions().map_err(|e| ValidationError::Execution(e.to_string()))?;
        let state_root = self
            .store
            .apply_account_updates_batch(block.header.parent_hash, &account_updates)
            .map_err(|e| ValidationError::Store(e.to_string()))?
            .ok_or(ValidationError::MissingParentState)?
            .state_trie_hash;

        if state_root != block.header.state_root {
            return Err(ValidationError::StateRootMismatch {
                got: b256(block.header.state_root),
                expected: b256(state_root),
            });
        }

        Ok(ExecutedBlock {
            block,
            parent_header: parent_header_for_reads,
            receipts: result.receipts,
            account_updates,
        })
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
