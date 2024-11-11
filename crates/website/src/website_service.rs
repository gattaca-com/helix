use crate::state::{AppState, CachedTemplates};
use axum::{routing::get, Router};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{debug, error, info, warn};

use crate::handlers;
use crate::models::DeliveredPayload;
use crate::postgres_db_website::WebsiteDatabaseService;
use crate::templates::IndexTemplate;
use helix_beacon_client::{
    beacon_client::BeaconClient, multi_beacon_client::MultiBeaconClient, MultiBeaconClientTrait,
};
use helix_common::chain_info::ChainInfo;
use helix_common::{NetworkConfig, RelayConfig};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use helix_housekeeper::{ChainEventUpdater, ChainUpdate};
use helix_utils::signing::compute_builder_domain;
use hex::encode as hex_encode;

pub struct WebsiteService {}

impl WebsiteService {
    pub async fn run(config: RelayConfig) -> Result<(), Box<dyn std::error::Error>> {
        info!("Starting WebsiteService");

        // Initialize PostgresDB
        let postgres_db = PostgresDatabaseService::from_relay_config(&config).unwrap();
        //postgres_db.run_migrations().await;
        //postgres_db.init_region(&config).await;
        let db = Arc::new(postgres_db);
        debug!("PostgresDB initialized");

        // ChainInfo
        let chain_info = Arc::new(match config.network_config {
            NetworkConfig::Mainnet => ChainInfo::for_mainnet(),
            NetworkConfig::Goerli => ChainInfo::for_goerli(),
            NetworkConfig::Sepolia => ChainInfo::for_sepolia(),
            NetworkConfig::Holesky => ChainInfo::for_holesky(),
            NetworkConfig::Custom { ref dir_path, ref genesis_validator_root, genesis_time } => {
                ChainInfo::for_custom(
                    dir_path.clone(),
                    genesis_validator_root.clone(),
                    genesis_time,
                )
                .expect("Failed to load custom chain info")
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
        let state = Arc::new(AppState {
            db_pool: db.clone(),
            chain_info: chain_info.clone(),
            website_config: config.website.clone(),
            cached_templates: Arc::new(RwLock::new(CachedTemplates {
                default: IndexTemplate::default(),
                by_value_desc: IndexTemplate::default(),
                by_value_asc: IndexTemplate::default(),
            })),
            latest_slot: Arc::new(RwLock::new(0)),
        });

        // Create the ChainEventUpdater and subscription
        let (mut chain_updater, chain_update_subscription) =
            ChainEventUpdater::new(db.clone(), chain_info.clone());
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
        let chain_update_subscription = chain_update_subscription.clone();
        tokio::spawn(async move {
            if let Err(e) =
                Self::handle_chain_updates(update_state, chain_update_subscription).await
            {
                error!("Error handling chain updates: {:?}", e);
            }
        });

        // Start website service
        let app = Router::new().route("/", get(handlers::index)).with_state(state);

        let addr: String =
            format!("{}:{}", config.website.listen_address, config.website.port).parse()?;
        let addr: SocketAddr = addr.parse().expect("Invalid listen address");
        let listener = TcpListener::bind(&addr).await.unwrap();
        info!("Website listening on {}", addr);

        axum::serve(listener, app).await.unwrap();

        Ok(())
    }

    async fn handle_chain_updates(
        state: Arc<AppState>,
        chain_update_subscription: mpsc::Sender<mpsc::Sender<ChainUpdate>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (tx, mut rx) = mpsc::channel(20);
        chain_update_subscription.send(tx).await?;

        info!("Subscribed to chain updates");

        while let Some(update) = rx.recv().await {
            match update {
                ChainUpdate::SlotUpdate(slot_update) => {
                    info!("Received slot update: {}", slot_update.slot);

                    // Update the latest slot using RwLock
                    {
                        let mut latest_slot = state.latest_slot.write().await;
                        *latest_slot = slot_update.slot;
                    }

                    debug!("Updated latest slot to {}", slot_update.slot);

                    // Update templates on new slot
                    if let Err(e) = Self::update_templates(&state).await {
                        error!("Error updating templates: {:?}", e);
                    }
                }
                ChainUpdate::PayloadAttributesUpdate(_) => {
                    // Not needed
                }
            }
        }
        warn!("Chain update handler exited");

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
        let mut cached_templates = state.cached_templates.write().await;
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
        let latest_slot = {
            let latest_slot = state.latest_slot.read().await;
            *latest_slot
        };

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
            latest_slot: latest_slot as i32,
            recent_payloads: recent_payloads.to_vec(),
            num_delivered_payloads,
            value_link: value_link.to_string(),
            value_order_icon: value_order_icon.to_string(),
            link_beaconchain: state.website_config.link_beaconchain.clone(),
            link_etherscan: state.website_config.link_etherscan.clone(),
            link_data_api: state.website_config.link_data_api.clone(),
            capella_fork_version: hex_encode(state.chain_info.context.capella_fork_version),
            bellatrix_fork_version: hex_encode(state.chain_info.context.bellatrix_fork_version),
            genesis_fork_version: hex_encode(state.chain_info.context.genesis_fork_version),
            genesis_validators_root: hex_encode(
                state.chain_info.genesis_validators_root.as_ref() as &[u8]
            ),
            builder_signing_domain: compute_builder_domain(&state.chain_info.context)
                .map(hex_encode)
                .unwrap_or_else(|_e| String::from("Error computing builder domain")),
        })
    }
}
