use std::{net::SocketAddr, sync::Arc};

use axum::{routing::get, Router};
use helix_beacon::{beacon_client::BeaconClient, multi_beacon_client::MultiBeaconClient};
use helix_common::{chain_info::ChainInfo, local_cache::LocalCache, NetworkConfig, RelayConfig};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use helix_housekeeper::{ChainEventUpdater, CurrentSlotInfo};
use parking_lot::RwLock;
use tokio::{net::TcpListener, sync::broadcast};
use tracing::{debug, error, info};

use crate::{
    handlers,
    models::DeliveredPayload,
    postgres_db_website::WebsiteDatabaseService,
    state::{AppState, CachedTemplates},
    templates::IndexTemplate,
};

pub struct WebsiteService;

impl WebsiteService {
    pub async fn run_loop(config: RelayConfig, db: Arc<PostgresDatabaseService>) {
        loop {
            match WebsiteService::run(config.clone(), db.clone()).await {
                Ok(_) => {
                    error!("Website service unexpectedly completed. Restarting...")
                }
                Err(e) => error!("Website server error: {}. Restarting...", e),
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    async fn run(
        config: RelayConfig,
        db: Arc<PostgresDatabaseService>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("Starting WebsiteService");

        // ChainInfo
        let chain_info = Arc::new(match config.network_config {
            NetworkConfig::Mainnet => ChainInfo::for_mainnet(),
            NetworkConfig::Sepolia => ChainInfo::for_sepolia(),
            NetworkConfig::Holesky => ChainInfo::for_holesky(),
            NetworkConfig::Hoodi => ChainInfo::for_hoodi(),
            NetworkConfig::Custom { ref dir_path, ref genesis_validator_root, genesis_time } => {
                ChainInfo::for_custom(dir_path.clone(), *genesis_validator_root, genesis_time)
            }
        });

        // Start a MultiBeaconClient
        let beacon_clients: Vec<Arc<BeaconClient>> = config
            .beacon_clients
            .iter()
            .map(|bc_config| Arc::new(BeaconClient::from_config(bc_config.clone())))
            .collect();
        let multi_beacon_client = Arc::new(MultiBeaconClient::new(beacon_clients));

        // Website state
        let current_slot_info = CurrentSlotInfo::new();
        let state = Arc::new(AppState {
            db_pool: db.clone(),
            chain_info: chain_info.clone(),
            website_config: config.website.clone(),
            cached_templates: Arc::new(RwLock::new(CachedTemplates {
                default: IndexTemplate::default(),
                by_value_desc: IndexTemplate::default(),
                by_value_asc: IndexTemplate::default(),
            })),
            current_slot_info: current_slot_info.clone(),
        });

        let (tx, _) = crossbeam_channel::bounded(0);
        let chain_updater = ChainEventUpdater::new(
            Arc::new(LocalCache::new_test()),
            chain_info,
            current_slot_info,
            tx,
        );
        info!("ChainEventUpdater initialized");

        let (head_event_tx, head_event_rx) = broadcast::channel(100);
        multi_beacon_client.subscribe_to_head_events(head_event_tx).await;
        let (payload_attributes_tx, payload_attributes_rx) = broadcast::channel(100);
        multi_beacon_client.subscribe_to_payload_attributes_events(payload_attributes_tx).await;

        // Spawn the ChainEventUpdater task
        tokio::spawn(async move {
            info!("Starting ChainEventUpdater");
            chain_updater.start(head_event_rx, payload_attributes_rx).await;
            error!("ChainEventUpdater unexpectedly stopped");
        });

        // Start handling chain updates
        let update_state = state.clone();
        tokio::spawn(async move {
            loop {
                // Update templates on new slot
                if let Err(err) = Self::update_templates(&update_state).await {
                    error!(%err, "error updating templates");
                };
                tokio::time::sleep(std::time::Duration::from_secs(12)).await;
            }
        });

        // Start website service
        let app = Router::new().route("/", get(handlers::index)).with_state(state);

        let addr: String =
            format!("{}:{}", config.website.listen_address, config.website.port).parse()?;
        let addr: SocketAddr = addr.parse().expect("Invalid listen address");
        let listener = TcpListener::bind(&addr).await?;
        info!("Website listening on {}", addr);

        axum::serve(listener, app).await?;

        Ok(())
    }

    async fn update_templates(state: &Arc<AppState>) -> Result<(), Box<dyn std::error::Error>> {
        debug!("Updating templates");

        // Fetch latest data
        let num_network_validators = match state.db_pool.get_num_network_validators().await {
            Ok(val) => val,
            Err(e) => {
                error!("Failed to get number of network validators: {:?}", e);
                return Err(Box::new(e));
            }
        };
        debug!("Fetched num_network_validators: {}", num_network_validators);

        let num_registered_validators = match state.db_pool.get_num_registered_validators().await {
            Ok(val) => val,
            Err(e) => {
                error!("Failed to get number of registered validators: {:?}", e);
                return Err(Box::new(e));
            }
        };
        debug!("Fetched num_registered_validators: {}", num_registered_validators);

        let recent_payloads = match state.db_pool.get_recent_delivered_payloads(30i64).await {
            Ok(val) => val,
            Err(e) => {
                error!("Failed to get recent delivered payloads: {:?}", e);
                return Err(Box::new(e));
            }
        };
        debug!("Fetched {} recent payloads", recent_payloads.len());

        let num_delivered_payloads = match state.db_pool.get_num_delivered_payloads().await {
            Ok(val) => val,
            Err(e) => {
                error!("Failed to get number of delivered payloads: {:?}", e);
                return Err(Box::new(e));
            }
        };
        debug!("Fetched num_delivered_payloads: {}", num_delivered_payloads);

        // Sort payloads for different views
        let mut payloads_by_value_desc = recent_payloads.clone();
        payloads_by_value_desc.sort_by(|a, b| b.bid_trace.value.cmp(&a.bid_trace.value));

        let mut payloads_by_value_asc = recent_payloads.clone();
        payloads_by_value_asc.sort_by(|a, b| a.bid_trace.value.cmp(&b.bid_trace.value));

        // Generate templates for different sorting orders
        let default_template = Self::generate_template(
            state,
            &recent_payloads,
            "",
            num_network_validators,
            num_registered_validators,
            num_delivered_payloads,
        )
        .await?;
        let by_value_desc_template = Self::generate_template(
            state,
            &payloads_by_value_desc,
            "-value",
            num_network_validators,
            num_registered_validators,
            num_delivered_payloads,
        )
        .await?;
        let by_value_asc_template = Self::generate_template(
            state,
            &payloads_by_value_asc,
            "value",
            num_network_validators,
            num_registered_validators,
            num_delivered_payloads,
        )
        .await?;

        // Update all cached templates
        let mut cached_templates = state.cached_templates.write();
        cached_templates.default = default_template;
        cached_templates.by_value_desc = by_value_desc_template;
        cached_templates.by_value_asc = by_value_asc_template;

        info!("Templates updated");
        Ok(())
    }

    async fn generate_template(
        state: &Arc<AppState>,
        recent_payloads: &[DeliveredPayload],
        order_by: &str,
        num_network_validators: i64,
        num_registered_validators: i64,
        num_delivered_payloads: i64,
    ) -> Result<IndexTemplate, Box<dyn std::error::Error>> {
        let latest_slot = state.current_slot_info.head_slot();

        let (value_link, value_order_icon) = match order_by {
            "-value" => ("/?order_by=value", "▼"),
            "value" => ("/", "▲"),
            _ => ("/?order_by=-value", ""),
        };

        Ok(IndexTemplate {
            network: state.website_config.network_name.to_string(),
            relay_url: state.website_config.relay_url.clone(),
            relay_pubkey: state.website_config.relay_pubkey.clone(),
            show_config_details: state.website_config.show_config_details,
            network_validators: num_network_validators,
            registered_validators: num_registered_validators,
            latest_slot: latest_slot.as_u64() as i32,
            recent_payloads: recent_payloads.to_vec(),
            num_delivered_payloads,
            value_link: value_link.to_string(),
            value_order_icon: value_order_icon.to_string(),
            link_beaconchain: state.website_config.link_beaconchain.clone(),
            link_etherscan: state.website_config.link_etherscan.clone(),
            link_data_api: state.website_config.link_data_api.clone(),
            capella_fork_version: alloy_primitives::hex::encode(
                state.chain_info.context.capella_fork_version,
            ),
            bellatrix_fork_version: alloy_primitives::hex::encode(
                state.chain_info.context.bellatrix_fork_version,
            ),
            genesis_fork_version: alloy_primitives::hex::encode(
                state.chain_info.context.genesis_fork_version,
            ),
            genesis_validators_root: alloy_primitives::hex::encode(
                state.chain_info.genesis_validators_root.as_ref() as &[u8],
            ),
            builder_signing_domain: state.chain_info.context.get_builder_domain().to_string(),
        })
    }
}
