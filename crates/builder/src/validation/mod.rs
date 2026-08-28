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
    types::{AccountUpdate, Block, BlockHeader, ELASTICITY_MULTIPLIER, Receipt},
    validation::{
        validate_block_pre_execution, validate_gas_used, validate_receipts_root_and_logs_bloom,
        validate_requests_hash,
    },
};
use ethrex_crypto::NativeCrypto;
use ethrex_storage::Store;
use tokio::sync::watch;

use crate::{
    engine::convert::{b256, payload_v3_to_block},
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
        self.execute(prepared)
    }

    /// Executes against the parent state and checks the header against what
    /// execution produced. Writes nothing to the store.
    pub fn execute(&self, prepared: PreparedBlock) -> Result<ExecutedBlock, ValidationError> {
        let PreparedBlock { block, parent_header } = prepared;
        let chain_config = self.store.get_chain_config();

        validate_block_pre_execution(&block, &parent_header, &chain_config, ELASTICITY_MULTIPLIER)
            .map_err(|e| ValidationError::PreExecution(e.to_string()))?;

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

        Ok(ExecutedBlock { block, receipts: result.receipts, account_updates })
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
