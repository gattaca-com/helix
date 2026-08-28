use clap::Parser;
use flux::{
    spine::FluxSpine,
    tile::{TileConfig, attach_tile},
    utils::ThreadPriority,
};
use tracing::info;
use tracing_subscriber::EnvFilter;

mod cli;
mod config;
mod engine;
mod node;
mod server;
mod spine;
mod utils;

use cli::BuilderCli;
use config::{MergingConfig, Roles, SimulationConfig};
use engine::{MergeEngine, types::EngineConfig};
use server::MergingServerTile;
use spine::BuilderSpine;

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> eyre::Result<()> {
    let cli = BuilderCli::parse();
    init_tracing(&cli);

    let merging_config = cli.merging_config.as_deref().map(MergingConfig::load).transpose()?;
    let simulation_config = cli.sim_config.as_deref().map(SimulationConfig::load).transpose()?;
    let roles = Roles::resolve(merging_config, simulation_config)?;

    // Fail fast on a missing/invalid RELAY_KEY, before the node boots.
    let relay_signer = roles.merging().map(|merging_config| {
        info!(listen_addr = %merging_config.listen_addr, "Loaded merging config");
        EngineConfig::load_relay_signer()
    });

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;

    let node = runtime.block_on(node::start(&cli.node))?;

    if let Some(simulation_config) = roles.simulation() {
        info!(
            ssz_addr = %simulation_config.ssz_addr,
            rpc_addr = %simulation_config.rpc_addr,
            "Simulation role active"
        );
    }

    // `BuilderSpine::start` blocks until its tiles stop, so merging starts last.
    if let Some(merging_config) = roles.merging() {
        let relay_signer = relay_signer.expect("the merging role loads a relay signer");

        let (event_tx, event_rx) = crossbeam_channel::bounded(merging_config.event_queue_capacity);
        let (output_tx, output_rx) = crossbeam_channel::bounded(64);

        let engine_config = EngineConfig {
            relay_signer,
            max_blocks_per_slot: merging_config.max_blocks_per_slot,
            max_orders_per_slot: merging_config.max_orders_per_slot as usize,
            min_value_increase_wei: alloy_primitives::U256::from(
                merging_config.emission.min_value_increase_wei,
            ),
            min_emission_interval: std::time::Duration::from_millis(
                merging_config.emission.min_interval_ms,
            ),
            core: merging_config.cores.merge_worker,
        };
        let _engine = MergeEngine::spawn(
            engine_config,
            node.store.clone(),
            node.blockchain.clone(),
            node.head.clone(),
            event_rx,
            output_tx,
        );

        BuilderSpine::remove_all_files();
        let spine = BuilderSpine::new(None);
        let server_tile_config = match merging_config.cores.server_tile {
            Some(core) => TileConfig::new(core, ThreadPriority::High),
            None => TileConfig::background(None, None),
        };
        spine.start(None, None, |spine| {
            let tile = MergingServerTile::new(merging_config, event_tx.clone(), output_rx.clone());
            attach_tile(tile, spine, server_tile_config);
        });
    }

    runtime.block_on(async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("Received SIGINT, shutting down"),
            _ = sigterm.recv() => info!("Received SIGTERM, shutting down"),
            _ = node.cancel_token.cancelled() => info!("Node cancelled, shutting down"),
        }
        node.shutdown().await;
    });

    Ok(())
}

fn init_tracing(cli: &BuilderCli) {
    let filter =
        EnvFilter::builder().with_default_directive(cli.node.log_level.into()).from_env_lossy();
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
