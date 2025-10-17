use alloy_primitives::{Address, U256};
use jsonrpsee::types::ErrorObject;
use reth_ethereum::{
    evm::primitives::block::BlockExecutionError,
    node::core::rpc::result::{internal_rpc_err, invalid_params_rpc_err},
    provider::ProviderError,
};
use reth_node_builder::NewPayloadError;
use reth_primitives::GotExpected;

use crate::validation::error::{GetParentError, ValidationApiError};

/// Errors thrown by the block merging API.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BlockMergingApiError {
    #[error("found an invalid signature in base block")]
    InvalidSignatureInBaseBlock,
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Execution(#[from] BlockExecutionError),
    #[error(transparent)]
    Payload(#[from] NewPayloadError),
    #[error("failed to create EvmEnv for next block")]
    NextEvmEnvFail,
    #[error("failed to create block context for next block")]
    BlockContext,
    #[error("failed to decode execution requests")]
    ExecutionRequests,
    #[error("no signer found for builder: {_0}")]
    NoSignerForBuilder(Address),
    #[error("could not find a proposer payment tx")]
    MissingProposerPayment,
    #[error("could not verify proposer payment tx")]
    InvalidProposerPayment,
    #[error("proposer payment delta mismatch: {_0}")]
    BuilderBalanceDeltaMismatch(GotExpected<U256>),
    #[error("no expected revenue from merged block")]
    ZeroMergedBlockRevenue,
    #[error("no expected revenue for winning builder")]
    ZeroRevenueForWinningBuilder,
    #[error("signer account is empty: {_0}")]
    EmptyBuilderSignerAccount(Address),
    #[error("signer ({address}) does not have enough balance: {current} < {required}")]
    NoBalanceInBuilderSigner { address: Address, current: U256, required: U256 },
    #[error("revenue allocation tx reverted")]
    RevenueAllocationReverted,
    #[error("reached blob limit")]
    BlobLimitReached,
    #[error("validation: {0}")]
    Validation(#[from] ValidationApiError),
    #[error("could not find parent block: {_0}")]
    GetParent(#[from] GetParentError),
    #[error("not enough gas for payment: {_0}")]
    NotEnoughGasForPayment(u64),
}

impl From<BlockMergingApiError> for ErrorObject<'static> {
    fn from(error: BlockMergingApiError) -> Self {
        match error {
            BlockMergingApiError::MissingProposerPayment |
            BlockMergingApiError::InvalidProposerPayment |
            BlockMergingApiError::NoSignerForBuilder(_) |
            BlockMergingApiError::NotEnoughGasForPayment(_) |
            BlockMergingApiError::InvalidSignatureInBaseBlock => {
                invalid_params_rpc_err(error.to_string())
            }

            BlockMergingApiError::GetParent(_) |
            BlockMergingApiError::BlobLimitReached |
            BlockMergingApiError::NextEvmEnvFail |
            BlockMergingApiError::BlockContext |
            BlockMergingApiError::RevenueAllocationReverted |
            BlockMergingApiError::ExecutionRequests |
            BlockMergingApiError::ZeroRevenueForWinningBuilder |
            BlockMergingApiError::ZeroMergedBlockRevenue |
            BlockMergingApiError::EmptyBuilderSignerAccount(_) |
            BlockMergingApiError::NoBalanceInBuilderSigner { .. } |
            BlockMergingApiError::BuilderBalanceDeltaMismatch(_) |
            BlockMergingApiError::Provider(_) => internal_rpc_err(error.to_string()),

            BlockMergingApiError::Execution(err) => match err {
                error @ BlockExecutionError::Validation(_) => {
                    invalid_params_rpc_err(error.to_string())
                }
                error @ BlockExecutionError::Internal(_) => internal_rpc_err(error.to_string()),
            },
            BlockMergingApiError::Payload(err) => match err {
                error @ NewPayloadError::Eth(_) => invalid_params_rpc_err(error.to_string()),
                error @ NewPayloadError::Other(_) => internal_rpc_err(error.to_string()),
            },
            BlockMergingApiError::Validation(err) => err.into(),
        }
    }
}
