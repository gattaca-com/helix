use std::sync::Arc;

use reth_ethereum::{
    Block, EthPrimitives,
    consensus::{ConsensusError, FullConsensus},
    node::EthereumNode,
    provider::db::DatabaseEnv,
    provider::providers::BlockchainProvider,
};
use reth_ethereum_engine_primitives::EthPayloadTypes;
use reth_node_builder::{NodeTypesWithDBAdapter, PayloadValidator};

pub type RethProvider = BlockchainProvider<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>;
// can we get more concrete?
pub type RethConsensus = dyn FullConsensus<EthPrimitives, Error = ConsensusError>;
pub type RethPayloadValidator = dyn PayloadValidator<EthPayloadTypes, Block = Block>;
