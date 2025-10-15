use std::{collections::HashSet, fs::File, path::PathBuf};

use alloy_primitives::B256;
use clap::Parser;
use eyre::ensure;
use helix_types::{BlsKeypair, BlsPublicKey, BlsPublicKeyBytes, BlsSecretKey};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::{BuilderInfo, ValidatorPreferences, api::*, chain_info::ChainInfo};

static mut LOCAL_DEV: bool = false;

pub fn is_local_dev() -> bool {
    unsafe { LOCAL_DEV }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RelayConfig {
    pub instance_id: Option<String>,
    #[serde(default)]
    pub website: WebsiteConfig,
    pub postgres: PostgresConfig,
    pub simulators: Vec<SimulatorConfig>,
    #[serde(default)]
    pub beacon_clients: Vec<BeaconClientConfig>,
    #[serde(default)]
    pub relays: Vec<RelayGossipConfig>,
    #[serde(default)]
    pub relay_network: RelayNetworkConfig,
    #[serde(default)]
    pub builders: Vec<BuilderConfig>,
    #[serde(default)]
    pub network_config: NetworkConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub validator_preferences: ValidatorPreferences,
    #[serde(default)]
    pub router_config: RouterConfig,
    #[serde(default = "default_duration")]
    pub target_get_payload_propagation_duration_ms: u64,
    /// Configuration for block merging parameters.
    #[serde(default)]
    pub block_merging_config: BlockMergingConfig,
    #[serde(default)]
    pub primev_config: Option<PrimevConfig>,
    pub discord_webhook_url: Option<Url>,
    #[serde(default)]
    pub alerts_config: Option<AlertsConfig>,
    pub inclusion_list: Option<InclusionListConfig>,
    pub is_submission_instance: bool,
    pub is_registration_instance: bool,
    pub admin_token: String,
    #[serde(default)]
    is_local_dev: bool,
    /// Cores configuration, recommended to be set for production use
    pub cores: CoresConfig,
}

impl RelayConfig {
    pub fn empty_for_test() -> Self {
        Self {
            instance_id: Default::default(),
            website: Default::default(),
            postgres: Default::default(),
            simulators: Default::default(),
            beacon_clients: Default::default(),
            relays: Default::default(),
            relay_network: Default::default(),
            builders: Default::default(),
            network_config: Default::default(),
            logging: Default::default(),
            validator_preferences: Default::default(),
            router_config: Default::default(),
            target_get_payload_propagation_duration_ms: Default::default(),
            block_merging_config: Default::default(),
            primev_config: Default::default(),
            discord_webhook_url: Default::default(),
            alerts_config: Default::default(),
            inclusion_list: Default::default(),
            is_submission_instance: Default::default(),
            is_registration_instance: Default::default(),
            admin_token: Default::default(),
            is_local_dev: Default::default(),
            cores: CoresConfig {
                auctioneer: 1,
                tokio: vec![],
                sub_workers: vec![],
                reg_workers: vec![],
            },
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct WebsiteConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub listen_address: String,
    #[serde(default)]
    pub show_config_details: bool,
    #[serde(default)]
    pub network_name: String,
    #[serde(default)]
    pub relay_url: String,
    #[serde(default)]
    pub relay_pubkey: String,
    #[serde(default)]
    pub link_beaconchain: String,
    #[serde(default)]
    pub link_etherscan: String,
    #[serde(default)]
    pub link_data_api: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct CoresConfig {
    pub auctioneer: usize,
    pub tokio: Vec<usize>,
    /// Submissions / GetPayload
    pub sub_workers: Vec<usize>,
    /// Registrations
    pub reg_workers: Vec<usize>,
}

impl Default for WebsiteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 8080,
            listen_address: "0.0.0.0".to_string(),
            show_config_details: false,
            network_name: String::new(),
            relay_url: String::new(),
            relay_pubkey: String::new(),
            link_beaconchain: String::new(),
            link_etherscan: String::new(),
            link_data_api: String::new(),
        }
    }
}

pub fn load_config() -> RelayConfig {
    let start_config = StartConfig::parse();

    let file = File::open(&start_config.config)
        .unwrap_or_else(|_| panic!("unable to find config file: '{}'", start_config.config));

    let config: RelayConfig = serde_yaml::from_reader(file).expect("failed to parse config file");

    unsafe {
        LOCAL_DEV = config.is_local_dev;
    }

    config
}

pub fn load_keypair() -> BlsKeypair {
    let signing_key_str = std::env::var("RELAY_KEY").expect("could not find RELAY_KEY in env");
    let signing_key_bytes =
        alloy_primitives::hex::decode(signing_key_str).expect("invalid RELAY_KEY bytes");

    let signing_key = BlsSecretKey::deserialize(signing_key_bytes.as_slice())
        .expect("could not convert env signing key to SecretKey");

    let public_key = signing_key.public_key();

    BlsKeypair::from_components(public_key, signing_key)
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PostgresConfig {
    pub hostname: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub db_name: String,
    pub user: String,
    pub password: String,
    pub region: i16,
    pub region_name: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct BlockMergingConfig {
    /// Flag to enable this feature.
    #[serde(default = "default_bool::<false>")]
    pub is_enabled: bool,
    /// Maximum age of a merged bid before it is considered stale and discarded.
    #[serde(default = "default_u64::<250>")]
    pub max_merged_bid_age_ms: u64,
}

fn default_port() -> u16 {
    5432
}

pub const fn default_bool<const B: bool>() -> bool {
    B
}

pub const fn default_usize<const U: usize>() -> usize {
    U
}

pub const fn default_u64<const D: u64>() -> u64 {
    D
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AlertsConfig {
    pub telegram_bot_token: String,
    pub chat_id: i64,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct SimulatorConfig {
    pub url: String,
    #[serde(default = "default_namespace")]
    pub namespace: String,
    #[serde(default = "default_bool::<false>")]
    pub is_merging_simulator: bool,
    /// roughly number of cores on simulator
    #[serde(default = "default_usize::<32>")]
    pub max_concurrent_tasks: usize,
}

fn default_namespace() -> String {
    "flashbots".to_string()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BeaconClientConfig {
    pub url: Url,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RelayGossipConfig {
    pub url: String,
}

#[derive(Default, Serialize, Deserialize, Clone)]
pub struct RelayNetworkConfig {
    /// Whether functionality is enabled or not
    #[serde(default = "default_bool::<false>")]
    pub is_enabled: bool,
    /// Information on known peers
    #[serde(default)]
    pub peers: Vec<RelayNetworkPeerConfig>,
    /// Duration until the first cutoff point on the slot (t_1),
    /// when we compute an inclusion list based on the ones
    /// broadcasted by our peers and broadcast it to them.
    /// Should be lower than [`Self::cutoff_2_ms`]
    ///
    /// See the network's IL module documentation for more details.
    #[serde(default = "default_u64::<2000>")]
    pub cutoff_1_ms: u64,
    /// Duration until the second cutoff point in the slot (t_2),
    /// when we compute the final inclusion list for the slot.
    /// Should be higher than [`Self::cutoff_1_ms`]
    ///
    /// See the network's IL module documentation for more details.
    #[serde(default = "default_u64::<4000>")]
    pub cutoff_2_ms: u64,
}

impl RelayNetworkConfig {
    /// Validates config is sane
    pub fn validate(&self) {
        let mut peer_pubkeys = HashSet::with_capacity(self.peers.len());
        for peer in &self.peers {
            peer.validate();
            let pubkey = peer.pubkey;
            assert!(!peer_pubkeys.contains(&pubkey), "duplicate peer pubkey found: {pubkey}");
            peer_pubkeys.insert(pubkey);
        }
        assert!(self.cutoff_1_ms < self.cutoff_2_ms, "cutoff_1_ms must be less than cutoff_2_ms");
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RelayNetworkPeerConfig {
    /// The URL of the peer.
    /// A valid URL is of the form 'ws://<peer-url>'
    pub url: Url,
    /// The BLS public key of the peer, to verify its identity.
    pub pubkey: BlsPublicKeyBytes,
}

impl RelayNetworkPeerConfig {
    fn validate(&self) {
        // Verify serialized public key is valid
        let _deserialized_pubkey = BlsPublicKey::deserialize(self.pubkey.as_ref())
            .inspect_err(
                |e| error!(err=?e, pubkey=%self.pubkey, "failed to deserialize peer pubkey"),
            )
            .expect("pubkey should be valid");

        let has_ws_scheme = ["ws", "wss"].contains(&self.url.scheme());
        let has_port = self.url.port().is_some();

        assert!(has_ws_scheme || has_port, "peer URL must have ws/wss scheme or a specific port");
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BuilderConfig {
    pub pub_key: BlsPublicKeyBytes,
    pub builder_info: BuilderInfo,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub enum NetworkConfig {
    #[default]
    Mainnet,
    Sepolia,
    Holesky,
    Hoodi,
    Custom {
        dir_path: String,
        genesis_validator_root: B256,
        genesis_time: u64,
    },
}

impl NetworkConfig {
    pub fn to_chain_info(&self) -> ChainInfo {
        match self {
            NetworkConfig::Mainnet => ChainInfo::for_mainnet(),
            NetworkConfig::Sepolia => ChainInfo::for_sepolia(),
            NetworkConfig::Holesky => ChainInfo::for_holesky(),
            NetworkConfig::Hoodi => ChainInfo::for_hoodi(),
            NetworkConfig::Custom { dir_path, genesis_validator_root, genesis_time } => {
                ChainInfo::for_custom(dir_path.clone(), *genesis_validator_root, *genesis_time)
            }
        }
    }

    pub fn short_name(&self) -> &str {
        match self {
            NetworkConfig::Mainnet => "Mainnet",
            NetworkConfig::Sepolia => "Sepolia",
            NetworkConfig::Holesky => "Holesky",
            NetworkConfig::Hoodi => "Hoodi",
            NetworkConfig::Custom { .. } => "Custom",
        }
    }
}

impl std::fmt::Display for NetworkConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetworkConfig::Mainnet => write!(f, "mainnet"),
            NetworkConfig::Sepolia => write!(f, "sepolia"),
            NetworkConfig::Holesky => write!(f, "holesky"),
            NetworkConfig::Hoodi => write!(f, "hoodi"),
            NetworkConfig::Custom { dir_path, genesis_validator_root, genesis_time } => {
                write!(f, "custom ({dir_path}, {genesis_validator_root}, {genesis_time})")
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PrimevConfig {
    pub builder_url: String,
    pub builder_contract: String,
    pub validator_url: String,
    pub validator_contract: String,
}

#[derive(Default, Serialize, Deserialize, Clone)]
pub enum LoggingConfig {
    #[default]
    Console,
    File {
        dir_path: PathBuf,
        file_name: String,
        /// OpenTelemetry server URL
        otlp_server: Option<Url>,
    },
}

// FIXME
impl LoggingConfig {
    pub fn dir_path(&self) -> Option<PathBuf> {
        match self {
            LoggingConfig::Console => None,
            LoggingConfig::File { dir_path, .. } => Some(dir_path.clone()),
        }
    }
}

#[derive(Parser, Debug, Clone, Default, Serialize, Deserialize)]
#[clap(name = "basic")]
pub struct StartConfig {
    #[clap(long, default_value = "config.yml")]
    pub config: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RouterConfig {
    #[serde(default)]
    pub enabled_routes: Vec<RouteInfo>,
    /// On receipt of a shutdown signal, milliseconds to wait after shutting down health endpoint
    /// and before terminating.
    #[serde(default = "default_u64::<12_000>")]
    pub shutdown_delay_ms: u64,
}

impl RouterConfig {
    // Function to resolve condensed variants and replace them with real routes
    pub fn resolve_condensed_routes(&mut self) {
        if self.enabled_routes.is_empty() {
            // If no routes are enabled, enable all real routes
            self.extend([Route::BuilderApi, Route::ProposerApi, Route::DataApi]);
        } else if self.contains(Route::All) {
            // If All is present, replace it with all real routes
            self.remove(&Route::All);
            self.extend([Route::BuilderApi, Route::ProposerApi, Route::DataApi]);
        }

        // Replace BuilderApi, ProposerApi, DataApi with their real routes
        self.replace_condensed_with_real(Route::BuilderApi, &[
            Route::GetValidators,
            Route::SubmitBlock,
            Route::GetTopBid,
            Route::GetInclusionList,
        ]);

        self.replace_condensed_with_real(Route::ProposerApi, &[
            Route::Status,
            Route::RegisterValidators,
            Route::GetHeader,
            Route::GetPayload,
        ]);

        self.replace_condensed_with_real(Route::DataApi, &[
            Route::ProposerPayloadDelivered,
            Route::BuilderBidsReceived,
            Route::ValidatorRegistration,
        ]);
    }

    pub fn enable_relay_network(&mut self) {
        self.extend([Route::RelayNetwork]);
    }

    fn contains(&self, route: Route) -> bool {
        self.enabled_routes.iter().map(|x| x.route).collect::<HashSet<_>>().contains(&route)
    }

    fn remove(&mut self, route: &Route) {
        self.enabled_routes.retain(|x| x.route != *route);
    }

    fn extend(&mut self, routes: impl IntoIterator<Item = Route>) {
        for route in routes {
            if !self.contains(route) {
                self.enabled_routes.push(RouteInfo { route, rate_limit: None });
            }
        }
    }

    fn replace_condensed_with_real(&mut self, special_variant: Route, real_routes: &[Route]) {
        if self.contains(special_variant) {
            self.remove(&special_variant);
            self.extend(real_routes.iter().cloned());
        }
    }

    /// Validate routes, returns true if the bid sorter should be started
    pub fn validate_bid_sorter(&self) -> eyre::Result<bool> {
        let routes = self.enabled_routes.iter().map(|r| r.route).collect::<Vec<_>>();

        if routes.contains(&Route::All) {
            return Ok(true);
        }

        let is_get_header_instance =
            routes.contains(&Route::ProposerApi) || routes.contains(&Route::GetHeader);
        let is_submission_instance =
            routes.contains(&Route::BuilderApi) || routes.contains(&Route::SubmitBlock);

        if is_get_header_instance {
            ensure!(
                is_submission_instance,
                "relay is serving headers so should have submissions enabled"
            );
            ensure!(
                routes.contains(&Route::BuilderApi) || routes.contains(&Route::GetTopBid),
                "routes should have get_top_bid enabled"
            );

            Ok(true)
        } else if is_submission_instance {
            ensure!(
                is_get_header_instance,
                "relay is receiving blocks so should have get_header enabled"
            );
            ensure!(
                routes.contains(&Route::BuilderApi) || routes.contains(&Route::GetTopBid),
                "routes should have get_top_bid enabled"
            );

            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteInfo {
    pub route: Route,
    pub rate_limit: Option<RateLimitInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitInfo {
    // Interval after which one element of the quota is replenished in milliseconds
    pub replenish_ms: u64,
    // The quota size that defines how many requests can occur before being rate limited and
    // clients have to wait until the elements of the quota are replenished
    pub burst_size: u32,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum Route {
    All,
    BuilderApi,
    ProposerApi,
    DataApi,
    GetValidators,
    SubmitBlock,
    GetTopBid,
    Status,
    RegisterValidators,
    GetHeader,
    GetPayload,
    ProposerPayloadDelivered,
    BuilderBidsReceived,
    ValidatorRegistration,
    GetInclusionList,
    RelayNetwork,
}

impl Route {
    pub fn path(&self) -> String {
        match self {
            Route::GetValidators => format!("{PATH_BUILDER_API}{PATH_GET_VALIDATORS}"),
            Route::SubmitBlock => format!("{PATH_BUILDER_API}{PATH_SUBMIT_BLOCK}"),
            Route::GetTopBid => format!("{PATH_BUILDER_API}{PATH_GET_TOP_BID}"),
            Route::GetInclusionList => format!("{PATH_BUILDER_API}{PATH_GET_INCLUSION_LIST}"),
            Route::Status => format!("{PATH_PROPOSER_API}{PATH_STATUS}"),
            Route::RegisterValidators => format!("{PATH_PROPOSER_API}{PATH_REGISTER_VALIDATORS}"),
            Route::GetHeader => format!("{PATH_PROPOSER_API}{PATH_GET_HEADER}"),
            Route::GetPayload => format!("{PATH_PROPOSER_API}{PATH_GET_PAYLOAD}"),
            Route::ProposerPayloadDelivered => {
                format!("{PATH_DATA_API}{PATH_PROPOSER_PAYLOAD_DELIVERED}")
            }
            Route::BuilderBidsReceived => format!("{PATH_DATA_API}{PATH_BUILDER_BIDS_RECEIVED}"),
            Route::ValidatorRegistration => format!("{PATH_DATA_API}{PATH_VALIDATOR_REGISTRATION}"),
            Route::All => panic!("All is not a real route"),
            Route::BuilderApi => panic!("BuilderApi is not a real route"),
            Route::ProposerApi => panic!("ProposerApi is not a real route"),
            Route::DataApi => panic!("DataApi is not a real route"),
            Route::RelayNetwork => PATH_RELAY_NETWORK.to_string(),
        }
    }
}

fn default_duration() -> u64 {
    1000
}

#[derive(Clone, Deserialize, Serialize)]
pub struct InclusionListConfig {
    pub node_url: Url,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_router_config(routes: Vec<Route>) -> RouterConfig {
        RouterConfig {
            enabled_routes: routes
                .into_iter()
                .map(|route| RouteInfo { route, rate_limit: None })
                .collect(),
            shutdown_delay_ms: 12_000,
        }
    }

    #[test]
    fn test_validate_bid_sorter_empty_routes() {
        let config = create_router_config(vec![]);

        let result = config.validate_bid_sorter();
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[test]
    fn test_validate_bid_sorter_all_route() {
        let config = create_router_config(vec![Route::All]);

        let result = config.validate_bid_sorter();
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_validate_bid_sorter_valid_get_header_instance() {
        let config =
            create_router_config(vec![Route::GetHeader, Route::SubmitBlock, Route::GetTopBid]);

        let result = config.validate_bid_sorter();
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_validate_bid_sorter_valid_proposer_api_instance() {
        let config = create_router_config(vec![Route::ProposerApi, Route::BuilderApi]);

        let result = config.validate_bid_sorter();
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_validate_bid_sorter_get_header_without_submission() {
        let config = create_router_config(vec![Route::GetHeader, Route::GetTopBid]);

        let result = config.validate_bid_sorter();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("relay is serving headers so should have submissions enabled")
        );
    }

    #[test]
    fn test_validate_bid_sorter_submission_without_get_header() {
        let config = create_router_config(vec![Route::SubmitBlock, Route::GetTopBid]);

        let result = config.validate_bid_sorter();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("relay is receiving blocks so should have get_header enabled")
        );
    }

    #[test]
    fn test_validate_bid_sorter_get_header_without_top_bid() {
        let config = create_router_config(vec![Route::GetHeader, Route::SubmitBlock]);

        let result = config.validate_bid_sorter();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("routes should have get_top_bid enabled"));
    }

    #[test]
    fn test_validate_bid_sorter_submission_without_top_bid() {
        let config = create_router_config(vec![Route::SubmitBlock, Route::GetHeader]);

        let result = config.validate_bid_sorter();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("routes should have get_top_bid enabled"));
    }

    #[test]
    fn test_validate_bid_sorter_data_api_only() {
        let config = create_router_config(vec![Route::DataApi, Route::ProposerPayloadDelivered]);

        let result = config.validate_bid_sorter();
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }
}
