//! Integration test: run the housekeeper tile against real beacon nodes and print SlotUpdates.
//!
//! Usage:
//!   BEACON_URLS=http://localhost:5052 cargo run -p helix-relay --example housekeeper_live
//!
//! BEACON_URLS is a comma-separated list of beacon node HTTP URLs.
//! Stops on SIGINT (Ctrl+C) or SIGTERM.

use std::{sync::Arc, time::Duration};

use flux::{
    spine::FluxSpine,
    tile::{Tile, TileConfig, TileName, attach_tile},
};
use flux_utils::SharedVector;
use helix_common::{
    BeaconClientConfig, RelayConfig,
    beacon::{create_beacon_client, load_chain_info},
    local_cache::LocalCache,
    signing::RelaySigningContext,
};
use helix_database::postgres::postgres_db_service::{DbRequest, PendingBlockSubmissionValue};
use helix_relay::{
    DbHandle, HelixSpine, SlotUpdate, HousekeeperTile, RelayNetworkManager, SlotMsg,
};
use helix_types::{BlsKeypair, BlsSecretKey};
use url::Url;

struct SlotEventPrinter {
    slot_events: Arc<SharedVector<SlotUpdate>>,
}

impl Tile<HelixSpine> for SlotEventPrinter {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        adapter.consume(|msg: SlotMsg, _producers| {
            let Some(ev) = self.slot_events.get(msg.ix) else { return };
            println!(
                "SlotUpdate: bid_slot={} has_registration={} has_payload_attrs={} has_il={}",
                ev.bid_slot,
                ev.registration_data.is_some(),
                !ev.payload_attributes.is_empty(),
                ev.il.is_some(),
            );
        });
    }

    fn name(&self) -> TileName {
        TileName::from_str_truncate("slot-printer")
    }
}

fn main() {
    tracing_subscriber::fmt::init();

    let beacon_urls = std::env::var("BEACON_URLS")
        .expect("BEACON_URLS env var required (comma-separated beacon node URLs)");

    let beacon_clients: Vec<BeaconClientConfig> = beacon_urls
        .split(',')
        .map(|s| BeaconClientConfig { url: s.trim().parse::<Url>().expect("invalid beacon URL") })
        .collect();
    assert!(!beacon_clients.is_empty(), "at least one beacon URL required");

    let mut config = RelayConfig::empty_for_test();
    config.beacon_clients = beacon_clients;

    let beacon_client = create_beacon_client(&config);

    let runtime =
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();

    let chain_info = Arc::new(runtime.block_on(load_chain_info(&beacon_client)));

    // Dummy signing context — network is disabled, this won't be used.
    let sk = BlsSecretKey::deserialize(
        &alloy_primitives::hex::decode(
            "64496d4e301e541a6e1237d6ef13a8f8b8b6cb82be9d8ac90073a833dfc2af11",
        )
        .unwrap(),
    )
    .expect("valid test secret key");
    let keypair = BlsKeypair::from_components(sk.public_key(), sk);
    let signing_context = Arc::new(RelaySigningContext::new(keypair, chain_info.clone()));
    let relay_network_api = RelayNetworkManager::new(config.relay_network.clone(), signing_context);

    // Noop DbHandle — drain channel in background.
    let (db_tx, db_rx) = crossbeam_channel::bounded::<DbRequest>(10_000);
    let (db_batch_tx, db_batch_rx) =
        crossbeam_channel::bounded::<PendingBlockSubmissionValue>(10_000);
    std::thread::spawn(move || for _ in db_rx {});
    std::thread::spawn(move || for _ in db_batch_rx {});
    let db = DbHandle::new(db_tx, db_batch_tx);

    let local_cache = Arc::new(LocalCache::new());
    let slot_events = Arc::new(SharedVector::<SlotUpdate>::with_capacity(64));

    let (housekeeper_tile, _curr_slot_info) = HousekeeperTile::new(
        db,
        beacon_client,
        local_cache,
        &config,
        chain_info,
        slot_events.clone(),
        relay_network_api,
    );

    let printer = SlotEventPrinter { slot_events };

    HelixSpine::remove_all_files();
    let spine = HelixSpine::new(None);

    spine.start(None, Some(Duration::from_millis(500)), |spine| {
        attach_tile(housekeeper_tile, spine, TileConfig::background(None, None));
        attach_tile(printer, spine, TileConfig::background(None, None));
    });
}
