use reth_ethereum::{
    Block, EthPrimitives,
    consensus::FullConsensus,
    node::EthereumNode,
    provider::{db::DatabaseEnv, providers::BlockchainProvider},
};
use reth_ethereum_engine_primitives::EthPayloadTypes;
use reth_node_builder::{NodeTypesWithDBAdapter, PayloadValidator};

pub type RethProvider = BlockchainProvider<NodeTypesWithDBAdapter<EthereumNode, DatabaseEnv>>;
// can we get more concrete?
pub type RethConsensus = dyn FullConsensus<EthPrimitives>;
pub type RethPayloadValidator = dyn PayloadValidator<EthPayloadTypes, Block = Block>;
pub type RethEvmConfig =
    reth_ethereum::evm::EthEvmConfig<reth_ethereum::chainspec::ChainSpec, reth_ethereum::evm::factory::RethEvmFactory>;
