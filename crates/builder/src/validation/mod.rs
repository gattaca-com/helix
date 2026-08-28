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
use ethrex_common::types::{Block, BlockHeader};
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
