use std::{collections::HashMap, sync::Arc};

use alloy_primitives::Address;
use async_trait::async_trait;
use helix_types::{BlockMergeRequestV1, BlockMergeResponseV1};
use jsonrpsee::{proc_macros::rpc, types::ErrorObject};
use reth_ethereum::node::core::rpc::result::internal_rpc_err;
use tokio::sync::oneshot;

use crate::{
    block_merging::types::{BlockMergingConfig, DistributionConfig, PrivateKeySigner},
    validation::ValidationApi,
};

/// Block merging rpc interface.
#[rpc(server, namespace = "relay")]
pub trait BlockMergingApi {
    /// A Request to append mergeable transactions to a block.
    #[method(name = "mergeBlockV1")]
    async fn merge_block_v1(
        &self,
        request: BlockMergeRequestV1,
    ) -> jsonrpsee::core::RpcResult<BlockMergeResponseV1>;
}

/// The type that implements the block merging rpc trait
#[derive(Clone, Debug, derive_more::Deref)]
pub(crate) struct BlockMergingApi {
    #[deref]
    inner: Arc<BlockMergingApiInner>,
}

impl BlockMergingApi {
    /// Create a new instance of the [`BlockMergingApi`]
    pub fn new(validation: ValidationApi, config: BlockMergingConfig) -> Self {
        let BlockMergingConfig {
            relay_fee_recipient,
            disperse_address,
            distribution_config,
            validate_merged_blocks,
            builder_collateral_map,
        } = config;

        distribution_config.validate();

        let inner = Arc::new(BlockMergingApiInner {
            validation,
            relay_fee_recipient,
            builder_collateral_map,
            disperse_address,
            distribution_config,
            validate_merged_blocks,
        });

        Self { inner }
    }
}

pub(crate) struct BlockMergingApiInner {
    /// The validation API.
    pub(crate) validation: ValidationApi,
    /// The address to send relay revenue to.
    pub(crate) relay_fee_recipient: Address,
    /// Builder coinbase -> collateral signer. The base block coinbase will accrue fees and
    /// disperse from its collateral address
    pub(crate) builder_collateral_map: HashMap<Address, PrivateKeySigner>,
    /// Address of disperse contract.
    /// It must have a `disperseEther(address[],uint256[])` function.
    pub(crate) disperse_address: Address,
    /// Configuration for revenue distribution.
    pub(crate) distribution_config: DistributionConfig,
    /// Whether to validate merged blocks or not
    pub(crate) validate_merged_blocks: bool,
}

impl core::fmt::Debug for BlockMergingApiInner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BlockMergingApiInner").finish_non_exhaustive()
    }
}

#[async_trait]
impl BlockMergingApiServer for BlockMergingApi {
    /// A Request to append mergeable transactions to a block.
    async fn merge_block_v1(
        &self,
        request: BlockMergeRequestV1,
    ) -> jsonrpsee::core::RpcResult<BlockMergeResponseV1> {
        let this = self.clone();
        let (tx, rx) = oneshot::channel();

        self.validation.task_spawner.spawn_blocking(Box::pin(async move {
            let result = Self::_merge_block_v1(&this, request)
                .await
                .inspect_err(|e| {
                    tracing::warn!(target: "rpc::relay::block_merging", %e, "Error merging block");
                })
                .map_err(ErrorObject::from);
            let _ = tx.send(result);
        }));

        rx.await.map_err(|_| internal_rpc_err("Internal blocking task error"))?
    }
}
