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
