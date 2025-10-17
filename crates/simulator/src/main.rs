mod block_merging;
mod common;
mod inclusion;
mod state_recorder;
mod validation;

use std::{path::PathBuf, sync::Arc};

use alloy_primitives::{Address, Bytes};
use clap::Parser;
use reth_chain_state::CanonStateSubscriptions;
use reth_ethereum::{
    cli::{chainspec::EthereumChainSpecParser, interface::Cli},
    node::{EthereumEngineValidator, EthereumNode, node::EthereumAddOns},
    rpc::api::eth::RpcNodeCore,
};
use reth_node_builder::FullNodeComponents;
use validation::{ValidationApi, ValidationApiConfig};

use crate::{
    block_merging::{BlockMergingApi, BlockMergingApiServer, types::BlockMergingConfig},
    inclusion::{
        api::{InclusionExt, InclusionExtApiServer},
        inclusion_producer::inclusion_producer,
    },
    state_recorder::run_block_state_recorder,
    validation::BlockSubmissionValidationApiServer,
};

fn main() {
    Cli::<EthereumChainSpecParser, CliExt>::parse()
        .run(|builder, args| async move {
            let handle = builder
                .with_types::<EthereumNode>()
                .with_components(EthereumNode::components().map_pool(|pool| {
                    // TODO set cutom order on the pool?
                    pool
                }))
                .with_add_ons(EthereumAddOns::default())
                .extend_rpc_modules(move |ctx| {
                    if args.record_block_state {
                        // Start block state recorder
                        let notifications = ctx.provider().canonical_state_stream();
                        let block_record_dir = args.record_blocks_dir.clone();
                        tokio::spawn(run_block_state_recorder(notifications, block_record_dir));
                    }

                    if !args.enable_ext {
                        return Ok(());
                    }

                    let validation_api = ValidationApi::new(
                        ctx.node().provider.clone(),
                        Arc::new(ctx.node().consensus().clone()),
                        RpcNodeCore::evm_config(ctx.node()).clone(),
                        ValidationApiConfig::new(
                            args.blacklist_provider.clone().unwrap_or_default(),
                        ),
                        Box::new(ctx.node().task_executor.clone()),
                        Arc::new(EthereumEngineValidator::new(ctx.config().chain.clone())),
                    );
                    if args.enable_block_merging_ext {
                        let block_merging_api =
                            BlockMergingApi::new(validation_api.clone(), args.clone().into());
                        ctx.modules.merge_configured(block_merging_api.into_rpc())?;
                    }

                    ctx.modules.merge_configured(validation_api.into_rpc())?;

                    if args.enable_inclusion_ext {
                        // Handle to the transaction pool.
                        let pool = ctx.pool().clone();

                        // Block commit update stream.
                        let notifications = ctx.provider().canonical_state_stream();

                        // List publisher
                        let (publisher, published) =
                            tokio::sync::watch::channel(None::<Vec<Bytes>>);

                        tokio::spawn(inclusion_producer(pool, notifications, publisher));

                        let ext = InclusionExt { published };
                        // now we merge our extension namespace into all configured transports
                        ctx.modules.merge_configured(ext.into_rpc())?;
                    }

                    Ok(())
                })
                .launch()
                .await?;

            handle.wait_for_node_exit().await
        })
        .unwrap();
}

/// Our custom cli args extension that adds flags to reth default CLI.
#[derive(Debug, Clone, Default, clap::Args)]
struct CliExt {
    /// CLI flag to enable the validation extension and other enabled ones
    #[arg(long)]
    pub enable_ext: bool,

    #[arg(long, default_value = "http://localhost:3520/blacklist")]
    pub blacklist_provider: Option<String>,

    #[arg(long, default_value_t = false)]
    pub record_block_state: bool,

    #[arg(long, default_value = "/root/blocks")]
    pub record_blocks_dir: String,

    #[arg(long, default_value_t = true)]
    pub enable_inclusion_ext: bool,

    #[arg(long, default_value_t = true)]
    pub enable_block_merging_ext: bool,

    /// Path to a file with a mapping `builder coinbase -> collateral signer`.
    /// The base block coinbase will accrue fees and disperse from its
    /// collateral address
    #[arg(long)]
    pub builder_collateral_map_path: PathBuf,

    #[arg(long)]
    pub relay_fee_recipient: Address,

    #[arg(long)]
    pub disperse_address: Address,

    #[arg(long)]
    pub validate_merged_blocks: bool,
}

impl From<CliExt> for BlockMergingConfig {
    fn from(cli: CliExt) -> Self {
        let builder_collateral_path = std::fs::read_to_string(cli.builder_collateral_map_path)
            .expect("Failed to read builder collateral map from file");
        let builder_collateral_map = serde_json::from_str(&builder_collateral_path)
            .expect("Failed to parse builder collateral map");
        BlockMergingConfig {
            builder_collateral_map,
            relay_fee_recipient: cli.relay_fee_recipient,
            distribution_config: Default::default(),
            disperse_address: cli.disperse_address,
            validate_merged_blocks: cli.validate_merged_blocks,
        }
    }
}
