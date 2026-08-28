use std::{net::SocketAddr, path::Path};

use serde::Deserialize;
use uuid::Uuid;

/// Builder-owned merging configuration, loaded from YAML (`--merging.config`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergingConfig {
    /// Address the merging TCP server listens on for relay connections.
    pub listen_addr: SocketAddr,
    /// Allowlisted relay api keys (`MergerRegistrationV1.api_key`).
    pub api_keys: Vec<Uuid>,
    /// Advertised in `MergerAckV1` and enforced on ingest.
    #[serde(default = "default_max_orders_per_slot")]
    pub max_orders_per_slot: u32,
    /// Advertised in `MergerAckV1`; caps decompressed frame bodies.
    #[serde(default = "default_max_frame_bytes")]
    pub max_frame_bytes: u32,
    #[serde(default = "default_true")]
    pub supports_zstd: bool,
    #[serde(default = "default_socket_buf_size")]
    pub socket_buf_size: usize,
    /// A connection that has not completed registration within this window is dropped.
    #[serde(default = "default_handshake_timeout_ms")]
    pub handshake_timeout_ms: u64,
    /// A registered connection with no inbound traffic for this long is dropped.
    #[serde(default = "default_idle_disconnect_s")]
    pub idle_disconnect_s: u64,
    /// Mergeable blocks admitted per slot before `LimitExceeded`.
    #[serde(default = "default_max_blocks_per_slot")]
    pub max_blocks_per_slot: usize,
    /// Capacity of the tile -> engine event queue; overflow answers `Busy`.
    #[serde(default = "default_event_queue_capacity")]
    pub event_queue_capacity: usize,
    #[serde(default)]
    pub cores: CoreConfig,
    #[serde(default)]
    pub emission: EmissionConfig,
}

/// Optional core pins; unpinned when absent.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoreConfig {
    pub server_tile: Option<usize>,
    pub merge_worker: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmissionConfig {
    /// A merged block is only emitted when its proposer value exceeds the last
    /// emission for the same base by more than this many wei.
    #[serde(default)]
    pub min_value_increase_wei: u128,
    /// Minimum spacing between emissions for the same base block.
    #[serde(default = "default_min_interval_ms")]
    pub min_interval_ms: u64,
}

impl Default for EmissionConfig {
    fn default() -> Self {
        Self { min_value_increase_wei: 0, min_interval_ms: default_min_interval_ms() }
    }
}

impl MergingConfig {
    pub fn load(path: &Path) -> eyre::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| eyre::eyre!("failed to read merging config {}: {e}", path.display()))?;
        let config: Self = serde_yaml::from_str(&raw)
            .map_err(|e| eyre::eyre!("failed to parse merging config {}: {e}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> eyre::Result<()> {
        if self.api_keys.is_empty() {
            eyre::bail!("merging config: api_keys must not be empty");
        }
        if self.max_frame_bytes < 1024 * 1024 {
            eyre::bail!("merging config: max_frame_bytes must be at least 1 MiB");
        }
        if self.max_orders_per_slot == 0 || self.max_blocks_per_slot == 0 {
            eyre::bail!("merging config: max_orders_per_slot and max_blocks_per_slot must be > 0");
        }
        Ok(())
    }
}

/// Builder-owned simulation configuration, loaded from YAML (`--sim.config`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimulationConfig {
    pub ssz_addr: SocketAddr,
    pub rpc_addr: SocketAddr,
    #[serde(default = "default_blacklist_endpoint")]
    pub blacklist_endpoint: String,
    /// Maximum parent-to-head block distance a submission may build on.
    #[serde(default = "default_validation_window")]
    pub validation_window: u64,
    #[serde(default = "default_max_concurrent_validations")]
    pub max_concurrent_validations: usize,
}

impl SimulationConfig {
    pub fn load(path: &Path) -> eyre::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| eyre::eyre!("failed to read simulation config {}: {e}", path.display()))?;
        let config: Self = serde_yaml::from_str(&raw).map_err(|e| {
            eyre::eyre!("failed to parse simulation config {}: {e}", path.display())
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> eyre::Result<()> {
        if self.ssz_addr == self.rpc_addr {
            eyre::bail!("simulation config: ssz_addr and rpc_addr must differ");
        }
        if self.blacklist_endpoint.is_empty() {
            eyre::bail!("simulation config: blacklist_endpoint must not be empty");
        }
        if self.validation_window == 0 {
            eyre::bail!("simulation config: validation_window must be > 0");
        }
        if self.max_concurrent_validations == 0 {
            eyre::bail!("simulation config: max_concurrent_validations must be > 0");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum Roles {
    Merging(MergingConfig),
    Simulation(SimulationConfig),
    Both { merging: MergingConfig, simulation: SimulationConfig },
}

impl Roles {
    pub fn resolve(
        merging: Option<MergingConfig>,
        simulation: Option<SimulationConfig>,
    ) -> eyre::Result<Self> {
        match (merging, simulation) {
            (Some(merging), Some(simulation)) => Ok(Self::Both { merging, simulation }),
            (Some(merging), None) => Ok(Self::Merging(merging)),
            (None, Some(simulation)) => Ok(Self::Simulation(simulation)),
            (None, None) => {
                eyre::bail!("no role selected: supply --merging.config, --sim.config, or both")
            }
        }
    }

    pub fn merging(&self) -> Option<&MergingConfig> {
        match self {
            Self::Merging(merging) | Self::Both { merging, .. } => Some(merging),
            Self::Simulation(_) => None,
        }
    }

    pub fn simulation(&self) -> Option<&SimulationConfig> {
        match self {
            Self::Simulation(simulation) | Self::Both { simulation, .. } => Some(simulation),
            Self::Merging(_) => None,
        }
    }
}

fn default_blacklist_endpoint() -> String {
    "http://localhost:3520/blacklist".to_string()
}
fn default_validation_window() -> u64 {
    3
}
fn default_max_concurrent_validations() -> usize {
    num_cpus::get()
}
fn default_max_orders_per_slot() -> u32 {
    8192
}
fn default_max_frame_bytes() -> u32 {
    32 * 1024 * 1024
}
fn default_true() -> bool {
    true
}
fn default_socket_buf_size() -> usize {
    64 * 1024 * 1024
}
fn default_handshake_timeout_ms() -> u64 {
    3000
}
fn default_idle_disconnect_s() -> u64 {
    30
}
fn default_max_blocks_per_slot() -> usize {
    256
}
fn default_event_queue_capacity() -> usize {
    4096
}
fn default_min_interval_ms() -> u64 {
    25
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_config() {
        let example = include_str!("../config.example.yml");
        let config: MergingConfig = serde_yaml::from_str(example).unwrap();
        config.validate().unwrap();
        assert!(config.supports_zstd);
        assert_eq!(config.emission.min_interval_ms, 25);
    }

    #[test]
    fn minimal_config_gets_defaults() {
        let config: MergingConfig = serde_yaml::from_str(
            "listen_addr: \"0.0.0.0:9876\"\napi_keys: [\"00000000-0000-0000-0000-000000000001\"]\n",
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.max_orders_per_slot, 8192);
        assert_eq!(config.max_frame_bytes, 32 * 1024 * 1024);
        assert_eq!(config.handshake_timeout_ms, 3000);
        assert!(config.cores.server_tile.is_none());
    }
}

#[cfg(test)]
mod simulation_config_tests {
    use super::*;

    fn minimal_merging_config() -> MergingConfig {
        serde_yaml::from_str(
            "listen_addr: \"0.0.0.0:9876\"\napi_keys: [\"00000000-0000-0000-0000-000000000001\"]\n",
        )
        .expect("the minimal merging config must parse")
    }

    fn minimal_simulation_config() -> SimulationConfig {
        serde_yaml::from_str("ssz_addr: \"0.0.0.0:8552\"\nrpc_addr: \"0.0.0.0:8553\"\n")
            .expect("the minimal simulation config must parse")
    }

    #[test]
    fn parses_the_example_sim_config() {
        let example = include_str!("../sim-config.example.yml");
        let config: SimulationConfig = serde_yaml::from_str(example).unwrap();
        config.validate().unwrap();

        assert_eq!(config.ssz_addr, "0.0.0.0:8552".parse::<SocketAddr>().unwrap());
        assert_eq!(config.rpc_addr, "0.0.0.0:8553".parse::<SocketAddr>().unwrap());
        assert_eq!(config.blacklist_endpoint, "http://localhost:3520/blacklist");
        assert_eq!(config.validation_window, 3);
        assert_eq!(config.max_concurrent_validations, 32);
    }

    #[test]
    fn minimal_sim_config_gets_defaults() {
        let config = minimal_simulation_config();
        config.validate().unwrap();

        assert_eq!(config.blacklist_endpoint, "http://localhost:3520/blacklist");
        assert_eq!(config.validation_window, 3);
        assert_eq!(config.max_concurrent_validations, num_cpus::get());
    }

    #[test]
    fn sim_config_rejects_one_address_for_both_servers() {
        let config: SimulationConfig =
            serde_yaml::from_str("ssz_addr: \"0.0.0.0:8552\"\nrpc_addr: \"0.0.0.0:8552\"\n")
                .unwrap();

        assert!(config.validate().is_err(), "ssz_addr must differ from rpc_addr");
    }

    #[test]
    fn a_merging_config_alone_selects_the_merging_role() {
        let roles = Roles::resolve(Some(minimal_merging_config()), None).unwrap();

        assert!(roles.merging().is_some());
        assert!(roles.simulation().is_none());
    }

    #[test]
    fn a_sim_config_alone_selects_the_simulation_role() {
        let roles = Roles::resolve(None, Some(minimal_simulation_config())).unwrap();

        assert!(roles.simulation().is_some());
        assert!(roles.merging().is_none(), "no merging role means no RELAY_KEY is needed");
    }

    #[test]
    fn both_configs_select_both_roles() {
        let roles =
            Roles::resolve(Some(minimal_merging_config()), Some(minimal_simulation_config()))
                .unwrap();

        assert!(roles.merging().is_some());
        assert!(roles.simulation().is_some());
    }

    #[test]
    fn neither_config_is_a_startup_error() {
        assert!(Roles::resolve(None, None).is_err(), "the builder must run at least one role");
    }
}
