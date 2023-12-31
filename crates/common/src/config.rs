use clap::Parser;
use serde::{Deserialize, Serialize};
use std::{fs, collections::HashSet};
use helix_utils::request_encoding::Encoding;
use crate::api::proposer_api::ValidatorPreferences;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct RelayConfig {
    pub mongo: MongoConfig,
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
    pub fork_info: ForkInfoConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub run_housekeeper: bool,
    pub validator_preferences: ValidatorPreferences,
    pub router_config: RouterConfig,
}

impl RelayConfig {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let start_config = StartConfig::parse();
        let config_str = fs::read_to_string(start_config.config)?;
        let config: RelayConfig = serde_yaml::from_str(&config_str)?;
        Ok(config)
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct MongoConfig {
    pub url: String,
    pub db_name: String,
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
    Bloxroute(BloxrouteConfig),
    BeaconClient(BeaconClientConfig),
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FiberConfig {
    pub url: String,
    pub api_key: String,
    pub encoding: Encoding,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct BloxrouteConfig {
    pub base_url: String,
    pub endpoint: String,
    pub auth_header: String,
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
pub enum ForkInfoConfig {
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
            self.enabled_routes.extend([
                Route::BuilderApi,
                Route::ProposerApi,
                Route::DataApi,
            ]);
        }

        // Replace BuilderApi, ProposerApi, DataApi with their real routes
        self.replace_condensed_with_real(Route::BuilderApi, &[
            Route::GetValidators,
            Route::SubmitBlock,
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
    Status,
    RegisterValidators,
    GetHeader,
    GetPayload,
    ProposerPayloadDelivered,
    BuilderBidsReceived,
    ValidatorRegistration,
}

#[cfg(test)]
#[test]
fn test_config() {
    use crate::api::proposer_api::ValidatorPreferences;

    let mut config = RelayConfig::default();
    config.mongo.url = "mongodb://localhost:27017".to_string();
    config.mongo.db_name = "test".to_string();
    config.redis.url = "redis://localhost:6379".to_string();
    config.simulator.url = "http://localhost:8080".to_string();
    config.beacon_clients.push(BeaconClientConfig { url: "http://localhost:8080".to_string() });
    config.broadcasters.push(BroadcasterConfig::BeaconClient(BeaconClientConfig { url: "http://localhost:8080".to_string() }));
    config.fork_info = ForkInfoConfig::Mainnet;
    config.logging =
        LoggingConfig::File { dir_path: "hello".to_string(), file_name: "test".to_string() };
    config.validator_preferences = ValidatorPreferences { censoring: true };
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
            ].iter().cloned().collect(),
    };
    println!("{}", serde_yaml::to_string(&config).unwrap());
}
