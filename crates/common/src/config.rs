use crate::ValidatorPreferences;
use clap::Parser;
use helix_utils::request_encoding::Encoding;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs::File};

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct RelayConfig {
    pub postgres: PostgresConfig,
    pub redis: RedisConfig,
    #[serde(default)]
    pub broadcasters: Vec<BroadcasterConfig>,
    pub simulator: SimulatorConfig,
    #[serde(default)]
    pub beacon_clients: Vec<BeaconClientConfig>,
    #[serde(default)]
    pub relays: Vec<RelayGossipConfig>,
    #[serde(default)]
    pub network_config: NetworkConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    pub validator_preferences: ValidatorPreferences,
    pub router_config: RouterConfig,
    #[serde(default = "default_duration")]
    pub target_get_payload_propagation_duration_ms: u64,
}

impl RelayConfig {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let start_config = StartConfig::parse();
        let file = File::open(start_config.config)?;
        let config: RelayConfig = serde_yaml::from_reader(file)?;
        Ok(config)
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PostgresConfig {
    pub hostname: String,
    pub db_name: String,
    pub user: String,
    pub password: String,
    pub region: i16,
    pub region_name: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct RedisConfig {
    pub url: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub enum BroadcasterConfig {
    Fiber(FiberConfig),
    BeaconClient(BeaconClientConfig),
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FiberConfig {
    pub url: String,
    pub api_key: String,
    pub encoding: Encoding,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct SimulatorConfig {
    pub url: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct BeaconClientConfig {
    pub url: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RelayGossipConfig {
    pub url: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub enum NetworkConfig {
    #[default]
    Mainnet,
    Goerli,
    Sepolia,
    Holesky,
}

#[derive(Default, Serialize, Deserialize, Clone)]
pub enum LoggingConfig {
    #[default]
    Console,
    File {
        dir_path: String,
        file_name: String,
    },
}

#[derive(Parser, Debug, Clone, Default, Serialize, Deserialize)]
#[clap(name = "basic")]
pub struct StartConfig {
    #[clap(long, default_value = "config.yml")]
    pub config: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Default)]
pub struct RouterConfig {
    pub enabled_routes: HashSet<Route>,
}

impl RouterConfig {
    // Function to resolve condensed variants and replace them with real routes
    pub fn resolve_condensed_routes(&mut self) {
        if self.enabled_routes.contains(&Route::All) {
            // If All is present, replace it with all real routes
            self.enabled_routes.remove(&Route::All);
            self.enabled_routes.extend([Route::BuilderApi, Route::ProposerApi, Route::DataApi]);
        }

        // Replace BuilderApi, ProposerApi, DataApi with their real routes
        self.replace_condensed_with_real(
            Route::BuilderApi,
            &[
                Route::GetValidators,
                Route::SubmitBlock,
                Route::SubmitBlockOptimistic,
                Route::SubmitHeader,
            ],
        );

        self.replace_condensed_with_real(
            Route::ProposerApi,
            &[Route::Status, Route::RegisterValidators, Route::GetHeader, Route::GetPayload],
        );

        self.replace_condensed_with_real(
            Route::DataApi,
            &[
                Route::ProposerPayloadDelivered,
                Route::BuilderBidsReceived,
                Route::ValidatorRegistration,
            ],
        );
    }

    fn replace_condensed_with_real(&mut self, special_variant: Route, real_routes: &[Route]) {
        if self.enabled_routes.contains(&special_variant) {
            self.enabled_routes.remove(&special_variant);
            self.enabled_routes.extend(real_routes.iter().cloned());
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum Route {
    All,
    BuilderApi,
    ProposerApi,
    DataApi,
    GetValidators,
    SubmitBlock,
    SubmitBlockOptimistic,
    SubmitHeader,
    Status,
    RegisterValidators,
    GetHeader,
    GetPayload,
    ProposerPayloadDelivered,
    BuilderBidsReceived,
    ValidatorRegistration,
}

fn default_duration() -> u64 {
    1000
}

#[cfg(test)]
#[test]
fn test_config() {
    use crate::ValidatorPreferences;

    let mut config = RelayConfig::default();
    config.redis.url = "redis://localhost:6379".to_string();
    config.simulator.url = "http://localhost:8080".to_string();
    config.beacon_clients.push(BeaconClientConfig { url: "http://localhost:8080".to_string() });
    config.broadcasters.push(BroadcasterConfig::BeaconClient(
        BeaconClientConfig { url: "http://localhost:8080".to_string() }
    ));
    config.network_config = NetworkConfig::Mainnet;
    config.logging =
        LoggingConfig::File { dir_path: "hello".to_string(), file_name: "test".to_string() };
    config.validator_preferences = ValidatorPreferences { censoring: true, trusted_builders: None };
    config.router_config = RouterConfig {
        enabled_routes: [
            Route::GetValidators,
            Route::SubmitBlock,
            Route::BuilderBidsReceived,
            Route::ValidatorRegistration,
            Route::GetHeader,
            Route::GetPayload,
            Route::ProposerPayloadDelivered,
            Route::RegisterValidators,
            Route::Status,
        ]
        .iter()
        .cloned()
        .collect(),
    };
    println!("{}", serde_yaml::to_string(&config).unwrap());
}
