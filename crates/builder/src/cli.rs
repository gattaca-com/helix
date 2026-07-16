use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;
use ethrex_common::types::DEFAULT_BUILDER_GAS_CEIL;
use ethrex_config::networks::Network;
use ethrex_p2p::{
    discovery::INITIAL_LOOKUP_INTERVAL_MS, peer_table::TARGET_PEERS, sync::SyncMode,
    tx_broadcaster::BROADCAST_INTERVAL_MS, types::Node,
};
use tracing::Level;

#[derive(Parser)]
#[command(
    name = "helix-builder",
    about = "Helix block-merging builder: embedded ethrex node + relay-facing merging TCP server"
)]
pub struct BuilderCli {
    #[command(flatten)]
    pub node: NodeOptions,

    /// Path to the merging YAML config (listen address, relay api keys, limits)
    #[arg(long = "merging.config", env = "HELIX_BUILDER_MERGING_CONFIG")]
    pub merging_config: PathBuf,
}

/// Embedded-ethrex node options. Flag and env names match the upstream `ethrex`
/// binary where the option exists there, so ethrex's operator docs apply.
#[derive(Parser, Debug, Clone)]
pub struct NodeOptions {
    #[arg(
        long = "network",
        value_name = "GENESIS_FILE_PATH_OR_NAME",
        help = "Genesis json path, or a known network name (mainnet, sepolia, hoodi). Defaults to mainnet.",
        env = "ETHREX_NETWORK",
        value_parser = clap::value_parser!(Network),
        help_heading = "Node options"
    )]
    pub network: Option<Network>,
    #[arg(
        long = "datadir",
        value_name = "DATABASE_DIRECTORY",
        help = "Base directory for the database; a network suffix is appended. The value `memory` selects the in-memory backend.",
        env = "ETHREX_DATADIR",
        help_heading = "Node options"
    )]
    pub datadir: PathBuf,
    #[arg(long = "bootnodes", value_parser = clap::value_parser!(Node), value_name = "BOOTNODE_LIST", value_delimiter = ',', num_args = 1.., help = "Comma separated enode URLs for P2P discovery bootstrap.", env = "ETHREX_BOOTNODES", help_heading = "P2P options")]
    pub bootnodes: Vec<Node>,
    #[arg(long = "syncmode", default_value = "snap", value_parser = parse_sync_mode, help = "\"full\" or \"snap\".", env = "ETHREX_SYNCMODE", help_heading = "P2P options")]
    pub syncmode: SyncMode,
    #[arg(
        long = "rocksdb.block-cache-size",
        value_name = "BYTES",
        default_value_t = ethrex_storage::DEFAULT_ROCKSDB_BLOCK_CACHE_SIZE_BYTES,
        help = "RocksDB shared block cache size in bytes.",
        env = "ETHREX_ROCKSDB_BLOCK_CACHE_SIZE",
        help_heading = "Storage options"
    )]
    pub rocksdb_block_cache_size: usize,
    #[arg(
        long = "skip-genesis-validation",
        action = clap::ArgAction::SetTrue,
        help = "Trust a pre-existing datadir's genesis instead of recomputing its state root.",
        env = "ETHREX_SKIP_GENESIS_VALIDATION",
        help_heading = "Node options"
    )]
    pub skip_genesis_validation: bool,
    #[arg(long = "log.level", default_value_t = Level::INFO, value_name = "LOG_LEVEL", help = "info, debug, trace, warn or error", env = "ETHREX_LOG_LEVEL", help_heading = "Node options")]
    pub log_level: Level,

    #[arg(
        long = "http.addr",
        default_value = "127.0.0.1",
        value_name = "ADDRESS",
        help = "Listening address for the http rpc server.",
        env = "ETHREX_HTTP_ADDR",
        help_heading = "RPC options"
    )]
    pub http_addr: String,
    #[arg(
        long = "http.port",
        default_value = "8545",
        value_name = "PORT",
        help = "Listening port for the http rpc server.",
        env = "ETHREX_HTTP_PORT",
        help_heading = "RPC options"
    )]
    pub http_port: String,
    #[arg(
        long = "http.api",
        default_value = "eth,net,web3",
        value_name = "NAMESPACES",
        value_delimiter = ',',
        value_parser = parse_http_namespace,
        help = "Comma-separated JSON-RPC namespaces enabled over HTTP.",
        env = "ETHREX_HTTP_API",
        help_heading = "RPC options"
    )]
    pub http_api: Vec<ethrex_rpc::RpcNamespace>,
    #[arg(
        long = "authrpc.addr",
        default_value = "127.0.0.1",
        value_name = "ADDRESS",
        help = "Listening address for the authenticated (Engine API) rpc server.",
        env = "ETHREX_AUTHRPC_ADDR",
        help_heading = "RPC options"
    )]
    pub authrpc_addr: String,
    #[arg(
        long = "authrpc.port",
        default_value = "8551",
        value_name = "PORT",
        help = "Listening port for the authenticated (Engine API) rpc server.",
        env = "ETHREX_AUTHRPC_PORT",
        help_heading = "RPC options"
    )]
    pub authrpc_port: String,
    #[arg(
        long = "authrpc.jwtsecret",
        default_value = "jwt.hex",
        value_name = "JWTSECRET_PATH",
        help = "Path to the jwt secret for authenticated rpc requests (created if missing).",
        env = "ETHREX_AUTHRPC_JWTSECRET_PATH",
        help_heading = "RPC options"
    )]
    pub authrpc_jwtsecret: String,

    #[arg(long = "p2p.disabled", action = clap::ArgAction::SetTrue, help = "Disable devp2p (no discovery, no snap sync; the node then only advances via the Engine API).", env = "ETHREX_P2P_DISABLED", help_heading = "P2P options")]
    pub p2p_disabled: bool,
    #[arg(
        long = "p2p.addr",
        value_name = "ADDRESS",
        help = "Bind address for the P2P protocol.",
        env = "ETHREX_P2P_ADDR",
        help_heading = "P2P options"
    )]
    pub p2p_addr: Option<String>,
    #[arg(
        long = "nat.extip",
        value_name = "IP",
        help = "External IP address to announce to peers.",
        env = "ETHREX_P2P_NAT_EXTIP",
        help_heading = "P2P options"
    )]
    pub nat_extip: Option<String>,
    #[arg(
        long = "p2p.port",
        default_value = "30303",
        value_name = "PORT",
        help = "TCP port for the P2P protocol.",
        env = "ETHREX_P2P_PORT",
        help_heading = "P2P options"
    )]
    pub p2p_port: String,
    #[arg(
        long = "discovery.port",
        default_value = "30303",
        value_name = "PORT",
        help = "UDP port for P2P discovery.",
        env = "ETHREX_P2P_DISCOVERY_PORT",
        help_heading = "P2P options"
    )]
    pub discovery_port: String,
    #[arg(long = "p2p.discv4", default_value_t = true, action = clap::ArgAction::Set, help = "Enable discv4 discovery.", help_heading = "P2P options")]
    pub discv4_enabled: bool,
    #[arg(long = "p2p.discv5", default_value_t = true, action = clap::ArgAction::Set, help = "Enable discv5 discovery.", help_heading = "P2P options")]
    pub discv5_enabled: bool,
    #[arg(long = "p2p.target-peers", default_value_t = TARGET_PEERS, value_name = "MAX_PEERS", help = "Max amount of connected peers.", env = "ETHREX_P2P_TARGET_PEERS", help_heading = "P2P options")]
    pub target_peers: usize,
    #[arg(long = "p2p.tx-broadcasting-interval", default_value_t = BROADCAST_INTERVAL_MS, value_name = "INTERVAL_MS", help = "Transaction broadcasting batching interval (ms).", env = "ETHREX_P2P_TX_BROADCASTING_INTERVAL", help_heading = "P2P options")]
    pub tx_broadcasting_time_interval: u64,
    #[arg(long = "p2p.lookup-interval", default_value_t = INITIAL_LOOKUP_INTERVAL_MS, value_name = "INITIAL_LOOKUP_INTERVAL", help = "Initial discovery lookup interval (ms).", env = "ETHREX_P2P_LOOKUP_INTERVAL", help_heading = "P2P options")]
    pub lookup_interval: f64,

    #[arg(
        long = "builder.extra-data",
        default_value = "helix-builder",
        value_name = "EXTRA_DATA",
        help = "Block extra data message for locally built payloads.",
        env = "ETHREX_BUILDER_EXTRA_DATA",
        help_heading = "Block building options"
    )]
    pub extra_data: String,
    #[arg(long = "builder.gas-limit", default_value_t = DEFAULT_BUILDER_GAS_CEIL, value_name = "GAS_LIMIT", help = "Target block gas limit.", env = "ETHREX_BUILDER_GAS_LIMIT", help_heading = "Block building options")]
    pub gas_limit: u64,
    #[arg(long = "builder.max-blobs", value_name = "MAX_BLOBS", help = "Maximum blobs per block for local building.", env = "ETHREX_BUILDER_MAX_BLOBS", value_parser = clap::value_parser!(u32).range(1..), help_heading = "Block building options")]
    pub max_blobs_per_block: Option<u32>,
    #[arg(
        long = "mempool.maxsize",
        default_value_t = 10_000,
        value_name = "MEMPOOL_MAX_SIZE",
        help = "Maximum size of the mempool in number of transactions.",
        env = "ETHREX_MEMPOOL_MAX_SIZE",
        help_heading = "Node options"
    )]
    pub mempool_max_size: usize,

    #[arg(long = "metrics", action = clap::ArgAction::SetTrue, help = "Enable the prometheus metrics server.", env = "ETHREX_METRICS", help_heading = "Node options")]
    pub metrics_enabled: bool,
    #[arg(
        long = "metrics.addr",
        value_name = "ADDRESS",
        default_value = "0.0.0.0",
        env = "ETHREX_METRICS_ADDR",
        help_heading = "Node options"
    )]
    pub metrics_addr: String,
    #[arg(
        long = "metrics.port",
        value_name = "PROMETHEUS_METRICS_PORT",
        default_value = "9090",
        env = "ETHREX_METRICS_PORT",
        help_heading = "Node options"
    )]
    pub metrics_port: String,
}

fn parse_sync_mode(s: &str) -> eyre::Result<SyncMode> {
    match s {
        "full" => Ok(SyncMode::Full),
        "snap" => Ok(SyncMode::Snap),
        other => Err(eyre::eyre!("Invalid syncmode {other:?}, expected either snap or full")),
    }
}

fn parse_http_namespace(s: &str) -> eyre::Result<ethrex_rpc::RpcNamespace> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(eyre::eyre!("empty namespace in --http.api"));
    }
    if trimmed.eq_ignore_ascii_case("engine") {
        return Err(eyre::eyre!(
            "`engine` cannot be enabled on --http.api; it is served on the authenticated RPC port"
        ));
    }
    ethrex_rpc::RpcNamespace::from_prefix(&trimmed.to_ascii_lowercase())
        .ok_or_else(|| eyre::eyre!("unknown RPC namespace {trimmed:?}"))
}

pub fn parse_socket_addr(addr: &str, port: &str) -> eyre::Result<SocketAddr> {
    use std::net::ToSocketAddrs;
    format!("{addr}:{port}")
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| eyre::eyre!("failed to resolve socket address {addr}:{port}"))
}
