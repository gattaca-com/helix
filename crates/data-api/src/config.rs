use helix_common::{LoggingConfig, PostgresConfig, RouterConfig, ValidatorPreferences};
use reqwest::Url;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct DataApiConfig {
    pub postgres: PostgresConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub validator_preferences: ValidatorPreferences,
    #[serde(default)]
    pub router_config: RouterConfig,
    #[serde(default = "default_api_port")]
    pub api_port: u16,
    pub discord_webhook_url: Option<Url>,
}

fn default_api_port() -> u16 {
    4040
}
