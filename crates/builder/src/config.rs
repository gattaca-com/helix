use std::{net::SocketAddr, path::Path};

use serde::Deserialize;
use uuid::Uuid;

/// Intrinsic gas of a plain transfer, the floor for the payout reserve.
const TX_GAS_COST: u64 = 21_000;
/// Consensus limit on the header's `extra_data`.
const MAX_EXTRA_DATA_BYTES: usize = 32;

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

/// Builder-owned building configuration, loaded from YAML (`--build.config`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildingConfig {
    /// Relay base URL. Serves proposer duties and accepts submissions.
    pub relay_url: String,
    /// Sent as `X-Api-Key` on every submission.
    pub api_key: String,
    /// Beacon node base URL, for the `payload_attributes` SSE topic.
    pub beacon_url: String,
    /// Added to every bid. The relay rejects a zero-value block, so without
    /// this an idle chain produces nothing.
    // Read once the payout is built.
    #[allow(dead_code)]
    #[serde(default = "default_subsidy_wei")]
    pub subsidy_wei: u128,
    /// Gas held back from the fill for the trailing payout transaction.
    #[serde(default = "default_payout_gas_reserve")]
    pub payout_gas_reserve: u64,
    #[serde(default = "default_extra_data")]
    pub extra_data: String,
    /// Points into the slot, in milliseconds, at which to build and submit.
    #[serde(default = "default_submit_offsets_ms")]
    pub submit_offsets_ms: Vec<u64>,
    /// Validate our own block before submitting it.
    // Read once the slot loop exists.
    #[allow(dead_code)]
    #[serde(default = "default_true")]
    pub self_validate: bool,
}

impl BuildingConfig {
    pub fn load(path: &Path) -> eyre::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| eyre::eyre!("failed to read building config {}: {e}", path.display()))?;
        let config: Self = serde_yaml::from_str(&raw)
            .map_err(|e| eyre::eyre!("failed to parse building config {}: {e}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> eyre::Result<()> {
        check_http_url("relay_url", &self.relay_url)?;
        check_http_url("beacon_url", &self.beacon_url)?;
        if self.api_key.is_empty() {
            eyre::bail!("building config: api_key must not be empty");
        }
        // A plain transfer cannot cost less than the intrinsic gas.
        if self.payout_gas_reserve < TX_GAS_COST {
            eyre::bail!("building config: payout_gas_reserve must be at least {TX_GAS_COST}");
        }
        if self.submit_offsets_ms.is_empty() {
            eyre::bail!("building config: submit_offsets_ms must not be empty");
        }
        if self.extra_data.len() > MAX_EXTRA_DATA_BYTES {
            eyre::bail!("building config: extra_data must be at most {MAX_EXTRA_DATA_BYTES} bytes");
        }
        Ok(())
    }
}

/// The roles the builder runs, one per supplied config. At least one is
/// required; any combination is allowed.
#[derive(Debug, Clone)]
pub struct Roles {
    merging: Option<MergingConfig>,
    simulation: Option<SimulationConfig>,
    building: Option<BuildingConfig>,
}

impl Roles {
    pub fn resolve(
        merging: Option<MergingConfig>,
        simulation: Option<SimulationConfig>,
        building: Option<BuildingConfig>,
    ) -> eyre::Result<Self> {
        if merging.is_none() && simulation.is_none() && building.is_none() {
            eyre::bail!(
                "no role selected: supply --merging.config, --sim.config, --build.config, or any combination"
            );
        }
        Ok(Self { merging, simulation, building })
    }

    pub fn merging(&self) -> Option<&MergingConfig> {
        self.merging.as_ref()
    }

    pub fn simulation(&self) -> Option<&SimulationConfig> {
        self.simulation.as_ref()
    }

    pub fn building(&self) -> Option<&BuildingConfig> {
        self.building.as_ref()
    }
}

/// `Url::parse` alone accepts `localhost:4040`, reading the host as a scheme.
fn check_http_url(field: &str, raw: &str) -> eyre::Result<()> {
    let url = reqwest::Url::parse(raw)
        .map_err(|e| eyre::eyre!("building config: {field} is not a URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        eyre::bail!("building config: {field} must be an http(s) URL with a host");
    }
    Ok(())
}

fn default_subsidy_wei() -> u128 {
    1_000_000_000_000_000
}
fn default_payout_gas_reserve() -> u64 {
    TX_GAS_COST
}
fn default_extra_data() -> String {
    "helix-builder".to_string()
}
fn default_submit_offsets_ms() -> Vec<u64> {
    vec![500, 2000]
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
const MINIMAL_BUILDING_YAML: &str = concat!(
    "relay_url: \"http://localhost:4040\"\n",
    "api_key: \"key\"\n",
    "beacon_url: \"http://localhost:3500\"\n",
);

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
        serde_yaml::from_str("ssz_addr: \"0.0.0.0:8552\"\n")
            .expect("the minimal simulation config must parse")
    }

    fn minimal_building_config() -> BuildingConfig {
        serde_yaml::from_str(MINIMAL_BUILDING_YAML).expect("the minimal building config must parse")
    }

    #[test]
    fn parses_the_example_sim_config() {
        let example = include_str!("../sim-config.example.yml");
        let config: SimulationConfig = serde_yaml::from_str(example).unwrap();
        config.validate().unwrap();

        assert_eq!(config.ssz_addr, "0.0.0.0:8552".parse::<SocketAddr>().unwrap());
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
    fn a_merging_config_alone_selects_the_merging_role() {
        let roles = Roles::resolve(Some(minimal_merging_config()), None, None).unwrap();

        assert!(roles.merging().is_some());
        assert!(roles.simulation().is_none());
        assert!(roles.building().is_none());
    }

    #[test]
    fn a_sim_config_alone_selects_the_simulation_role() {
        let roles = Roles::resolve(None, Some(minimal_simulation_config()), None).unwrap();

        assert!(roles.simulation().is_some());
        assert!(roles.merging().is_none(), "no merging role means no RELAY_KEY is needed");
        assert!(roles.building().is_none());
    }

    #[test]
    fn both_configs_select_both_roles() {
        let roles =
            Roles::resolve(Some(minimal_merging_config()), Some(minimal_simulation_config()), None)
                .unwrap();

        assert!(roles.merging().is_some());
        assert!(roles.simulation().is_some());
        assert!(roles.building().is_none());
    }

    #[test]
    fn a_build_config_alone_selects_the_building_role() {
        let roles = Roles::resolve(None, None, Some(minimal_building_config())).unwrap();

        assert!(roles.building().is_some());
        assert!(roles.merging().is_none(), "no merging role means no RELAY_KEY is needed");
        assert!(roles.simulation().is_none());
    }

    #[test]
    fn all_three_configs_select_all_three_roles() {
        let roles = Roles::resolve(
            Some(minimal_merging_config()),
            Some(minimal_simulation_config()),
            Some(minimal_building_config()),
        )
        .unwrap();

        assert!(roles.merging().is_some());
        assert!(roles.simulation().is_some());
        assert!(roles.building().is_some());
    }

    #[test]
    fn neither_config_is_a_startup_error() {
        assert!(
            Roles::resolve(None, None, None).is_err(),
            "the builder must run at least one role"
        );
    }
}

#[cfg(test)]
mod building_config_tests {
    use super::*;

    fn with_line(extra: &str) -> BuildingConfig {
        serde_yaml::from_str(&format!("{MINIMAL_BUILDING_YAML}{extra}\n"))
            .expect("the config must parse")
    }

    #[test]
    fn parses_the_example_build_config() {
        let example = include_str!("../build-config.example.yml");
        let config: BuildingConfig = serde_yaml::from_str(example).unwrap();
        config.validate().unwrap();

        assert_eq!(config.relay_url, "http://localhost:4040");
        assert_eq!(config.beacon_url, "http://localhost:3500");
        assert_eq!(config.subsidy_wei, 1_000_000_000_000_000);
        assert_eq!(config.payout_gas_reserve, 21_000);
        assert_eq!(config.extra_data, "helix-builder");
        assert_eq!(config.submit_offsets_ms, vec![500, 2000]);
        assert!(config.self_validate);
    }

    #[test]
    fn minimal_build_config_gets_defaults() {
        let config: BuildingConfig = serde_yaml::from_str(MINIMAL_BUILDING_YAML).unwrap();
        config.validate().unwrap();

        assert_eq!(
            config.subsidy_wei, 1_000_000_000_000_000,
            "an idle chain still gets a non-zero bid"
        );
        assert_eq!(config.payout_gas_reserve, 21_000);
        assert_eq!(config.extra_data, "helix-builder");
        assert_eq!(config.submit_offsets_ms, vec![500, 2000]);
        assert!(config.self_validate);
    }

    #[test]
    fn rejects_an_unknown_field() {
        let err = serde_yaml::from_str::<BuildingConfig>(&format!(
            "{MINIMAL_BUILDING_YAML}subsidy_we: 1\n"
        ))
        .expect_err("a misspelled field must not be silently ignored");

        assert!(err.to_string().contains("unknown field"), "got: {err}");
    }

    #[test]
    fn rejects_a_payout_gas_reserve_below_the_intrinsic_cost() {
        let err = with_line("payout_gas_reserve: 20999")
            .validate()
            .expect_err("a reserve below the intrinsic gas can never pay for a transfer");

        assert!(err.to_string().contains("payout_gas_reserve"), "got: {err}");
    }

    #[test]
    fn rejects_empty_submit_offsets() {
        let err = with_line("submit_offsets_ms: []")
            .validate()
            .expect_err("without an offset the role would start and never build");

        assert!(err.to_string().contains("submit_offsets_ms"), "got: {err}");
    }

    #[test]
    fn rejects_a_malformed_relay_url() {
        let config: BuildingConfig = serde_yaml::from_str(concat!(
            "relay_url: \"localhost:4040\"\n",
            "api_key: \"key\"\n",
            "beacon_url: \"http://localhost:3500\"\n",
        ))
        .unwrap();

        let err = config.validate().expect_err("fail at boot, not on first submission");
        assert!(err.to_string().contains("relay_url"), "got: {err}");
    }

    #[test]
    fn rejects_extra_data_over_32_bytes() {
        let err = with_line(&format!("extra_data: \"{}\"", "x".repeat(33)))
            .validate()
            .expect_err("extra_data must fit the header field");

        assert!(err.to_string().contains("extra_data"), "got: {err}");
    }

    #[test]
    fn a_zero_subsidy_is_allowed() {
        let config = with_line("subsidy_wei: 0");
        config.validate().expect("an operator may bid only what the block earns");

        assert_eq!(config.subsidy_wei, 0);
    }
}
