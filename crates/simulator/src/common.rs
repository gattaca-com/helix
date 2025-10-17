use std::sync::Arc;

use reth_ethereum::{
    Block, EthPrimitives,
    consensus::{ConsensusError, FullConsensus},
    evm::revm::{cached::CachedReadsDBRef, database::StateProviderDatabase},
    node::EthereumNode,
    provider::{db::DatabaseEnv, providers::BlockchainProvider},
    storage::StateProvider,
};
use reth_ethereum_engine_primitives::EthPayloadTypes;
use reth_node_builder::{NodeTypesWithDBAdapter, PayloadValidator};
use revm::database::{State, WrapDatabaseRef};

pub type RethProvider = BlockchainProvider<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>;
// can we get more concrete?
pub type RethConsensus = dyn FullConsensus<EthPrimitives, Error = ConsensusError>;
pub type RethPayloadValidator = dyn PayloadValidator<EthPayloadTypes, Block = Block>;
pub type CachedRethDb<'a> = State<
    WrapDatabaseRef<&'a CachedReadsDBRef<'a, StateProviderDatabase<&'a Box<dyn StateProvider>>>>,
>;
