use helix_common::{PostgresConfig, ValidatorPreferences};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct DataApiConfig {
    pub postgres: PostgresConfig,
    #[serde(default)]
    pub validator_preferences: ValidatorPreferences,
    #[serde(default = "default_api_port")]
    pub api_port: u16,
}

fn default_api_port() -> u16 {
    4040
}
