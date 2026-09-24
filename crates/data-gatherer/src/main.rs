use std::time::Duration;

use flux::tile::{TileConfig, attach_tile};
use helix_common::{
    load_config,
    task::{block_on, init_runtime},
    utils::{init_panic_hook, init_tracing_log, install_default_crypto_provider},
};
use helix_relay::{HelixSpine, RelayConfigExt};
use tikv_jemallocator::Jemalloc;
use tracing::info;

use crate::tile::{DataGatherer, SHUTDOWN_DRAIN};

mod clickhouse;
mod s3;
mod tile;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

fn main() {
    install_default_crypto_provider();

    let RelayConfigExt { config, spine_config } = load_config();
    init_runtime(&config);

    let instance_id = config
        .instance_id
        .clone()
        .unwrap_or_else(|| format!("RelayUnknown_{}", config.postgres.region_name));

    let _guard = block_on(init_tracing_log(
        &config.logging,
        &config.postgres.region_name,
        instance_id.clone(),
    ));

    init_panic_hook(
        instance_id.clone(),
        config.discord_webhook_url.clone(),
        config.logging.dir_path(),
    );

    let dg = &config.data_gather;
    if dg.addresses.is_empty() &&
        dg.persist_dir.is_none() &&
        dg.clickhouse.is_none() &&
        dg.s3.is_none()
    {
        info!("no data gather sinks configured, exiting");
        // Distinct from the epoch-change exit (0): the entrypoint stops restarting on 3.
        std::process::exit(3);
    }

    let gatherer = DataGatherer::new(instance_id, config.data_gather.clone());
    let core = config.cores.data_gatherer;

    let spine = if let Some(spine_config) = spine_config {
        HelixSpine::new_with_config(None, spine_config)
    } else {
        HelixSpine::new(None)
    };

    info!("data-gatherer starting");
    // Grace must cover teardown's bounded drain plus the disk drain, or the
    // fallback kills the process mid-flush.
    spine.start(None, Some(SHUTDOWN_DRAIN + Duration::from_secs(5)), |spine| {
        attach_tile(gatherer, spine, TileConfig::new(core, None));
    });
}
