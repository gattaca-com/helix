use helix_common::{LoggingConfig, PostgresConfig, ValidatorPreferences};
use reqwest::Url;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct AdminConfig {
    pub postgres: PostgresConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub validator_preferences: ValidatorPreferences,
    #[serde(default = "default_api_port")]
    pub api_port: u16,
    /// Base URL of the relay's admin API, e.g. http://relay:4050
    pub relay_admin_url: Url,
    pub discord_webhook_url: Option<Url>,
}

fn default_api_port() -> u16 {
    4060
}
