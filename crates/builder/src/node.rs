//! Embedded ethrex full node: store + blockchain + devp2p + Engine API.
//!
//! This replicates the wiring of ethrex's `init_l1` (`cmd/ethrex/initializers.rs`)
//! from the library crates. The `ethrex` cmd-lib itself is not a dependency (see
//! the note in the workspace manifest), and unlike `init_l1` this keeps the
//! `Arc<Blockchain>` so the merge engine can share the node's instance.

use std::{
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ethrex_blockchain::{Blockchain, BlockchainOptions, BlockchainType};
use ethrex_common::H256;
use ethrex_config::networks::Network;
use ethrex_p2p::{
    DiscoveryConfig,
    network::P2PContext,
    peer_handler::PeerHandler,
    peer_table::{PeerTable, PeerTableServer, PeerTableServerProtocol as _},
    sync::SyncMode,
    sync_manager::SyncManager,
    types::{NetworkConfig, Node, NodeRecord},
    utils::public_key_from_signing_key,
};
use ethrex_storage::{EngineType, Store, StoreConfig};
use secp256k1::SecretKey;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::{debug, error, info, warn};

use crate::cli::{NodeOptions, parse_socket_addr};

const HEAD_WATCH_INTERVAL: Duration = Duration::from_millis(25);
const NODE_CONFIG_FILENAME: &str = "node_config.json";

/// Snapshot of the node's canonical head, published by the head watcher.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeadInfo {
    pub number: u64,
    pub hash: H256,
    pub timestamp: u64,
    pub is_synced: bool,
}

pub struct NodeHandle {
    pub store: Store,
    pub blockchain: Arc<Blockchain>,
    pub cancel_token: CancellationToken,
    pub tracker: TaskTracker,
    pub head: watch::Receiver<HeadInfo>,
    pub datadir: PathBuf,
    peer_table: PeerTable,
    node_record: NodeRecord,
}

/// Boots the embedded ethrex node. Must be called from within a tokio runtime.
pub async fn start(opts: &NodeOptions) -> eyre::Result<NodeHandle> {
    let network = opts.network.clone().unwrap_or(Network::mainnet());
    let datadir = effective_datadir(&opts.datadir, &network);

    if let Err(err) = ethrex_common::fd_limit::raise_fd_limit() {
        warn!("Failed to raise fd limit: {err}");
    }

    if !is_memory_datadir(&datadir) {
        init_datadir(&datadir);
    }

    let genesis = network.get_genesis()?;
    info!(
        %network,
        chain_id = genesis.config.chain_id,
        genesis_hash = %genesis.get_block().hash(),
        datadir = %datadir.display(),
        "Initializing embedded ethrex node"
    );
    debug!("Preloading KZG trusted setup");
    ethrex_crypto::kzg::warm_up_trusted_setup();

    let store_config = StoreConfig {
        rocksdb_block_cache_size: opts.rocksdb_block_cache_size,
        ..StoreConfig::default()
    };
    let engine_type =
        if is_memory_datadir(&datadir) { EngineType::InMemory } else { EngineType::RocksDB };
    let mut store = Store::new_with_config(&datadir, engine_type, store_config)?;
    if opts.skip_genesis_validation {
        store.add_initial_state_skip_validation(genesis).await?;
    } else {
        store.add_initial_state(genesis).await?;
    }

    if opts.syncmode == SyncMode::Full {
        store.generate_flatkeyvalue()?;
    }

    let blockchain: Arc<Blockchain> = Blockchain::new(store.clone(), BlockchainOptions {
        max_mempool_size: opts.mempool_max_size,
        perf_logs_enabled: true,
        r#type: BlockchainType::L1,
        max_blobs_per_block: opts.max_blobs_per_block,
        ..Default::default()
    })
    .into();

    regenerate_head_state(&store, &blockchain).await?;

    let signer = get_signer(&datadir);
    let (local_p2p_node, network_config) = get_local_p2p_node(opts, &signer);
    let node_record = get_local_node_record(&datadir, &local_p2p_node, &signer);

    let peer_table =
        PeerTableServer::spawn(local_p2p_node.node_id(), opts.target_peers, store.clone());
    let tracker = TaskTracker::new();
    let cancel_token = CancellationToken::new();

    let p2p_context = P2PContext::new(
        local_p2p_node.clone(),
        network_config,
        tracker.clone(),
        signer,
        peer_table.clone(),
        store.clone(),
        blockchain.clone(),
        client_version_string(),
        None,
        opts.tx_broadcasting_time_interval,
        opts.lookup_interval,
    )
    .map_err(|e| eyre::eyre!("P2P context could not be created: {e}"))?;

    let initiator = ethrex_p2p::rlpx::initiator::RLPxInitiator::spawn(p2p_context.clone());
    let peer_handler = PeerHandler::new(peer_table.clone(), initiator);

    let syncer = SyncManager::new(
        peer_handler.clone(),
        &opts.syncmode,
        cancel_token.clone(),
        blockchain.clone(),
        store.clone(),
        datadir.clone(),
    )
    .await;

    let http_addr = parse_socket_addr(&opts.http_addr, &opts.http_port)?;
    let authrpc_addr = parse_socket_addr(&opts.authrpc_addr, &opts.authrpc_port)?;
    if http_addr == authrpc_addr {
        eyre::bail!("--http.addr/--http.port and --authrpc.addr/--authrpc.port must differ");
    }
    let jwt_secret = read_jwtsecret_file(&opts.authrpc_jwtsecret)?;

    let bound = ethrex_rpc::bind_api(
        cancel_token.clone(),
        http_addr,
        None,
        authrpc_addr,
        store.clone(),
        blockchain.clone(),
        jwt_secret,
        local_p2p_node.clone(),
        node_record.clone(),
        syncer,
        peer_handler.clone(),
        client_version(),
        None,
        opts.gas_limit,
        opts.extra_data.clone(),
        opts.http_api.iter().copied().collect(),
    )
    .await?;
    info!(%http_addr, %authrpc_addr, "RPC + Engine API bound");
    spawn_fatal(&tracker, cancel_token.clone(), "RPC server", bound.serve());

    if opts.metrics_enabled {
        info!("Starting metrics server on {}:{}", opts.metrics_addr, opts.metrics_port);
        ethrex_metrics::profiling::initialize_block_processing_profile();
        ethrex_metrics::rpc::initialize_rpc_metrics();
        let metrics_api = ethrex_metrics::api::start_prometheus_metrics_api(
            opts.metrics_addr.clone(),
            opts.metrics_port.clone(),
        );
        tracker.spawn(async move {
            if let Err(err) = metrics_api.await {
                error!("metrics server exited with error: {err}");
            }
        });
    }

    if opts.p2p_disabled {
        info!("P2P is disabled");
    } else {
        let mut bootnodes = opts.bootnodes.clone();
        bootnodes.extend(network.get_bootnodes());
        if let Ok(Some(mut node_config)) = read_node_config_file(&datadir) {
            bootnodes.append(&mut node_config.known_peers);
        }
        if bootnodes.is_empty() {
            warn!("No bootnodes specified. This node will not be able to connect to the network.");
        }
        let discovery_config = DiscoveryConfig {
            discv4_enabled: opts.discv4_enabled,
            discv5_enabled: opts.discv5_enabled,
            ..Default::default()
        };
        ethrex_p2p::start_network(p2p_context, bootnodes, discovery_config)
            .await
            .map_err(|e| eyre::eyre!("Network failed to start: {e}"))?;
        tracker.spawn(ethrex_p2p::periodically_show_peer_stats(
            blockchain.clone(),
            peer_handler.peer_table.clone(),
        ));
    }

    let head =
        spawn_head_watcher(store.clone(), blockchain.clone(), &tracker, cancel_token.clone()).await;

    Ok(NodeHandle {
        store,
        blockchain,
        cancel_token,
        tracker,
        head,
        datadir,
        peer_table,
        node_record,
    })
}

impl NodeHandle {
    /// Graceful shutdown, mirroring the upstream binary: cancel subsystems,
    /// persist known peers, close the task tracker, fsync the store.
    pub async fn shutdown(self) {
        info!("Shutting down embedded node");
        self.cancel_token.cancel();

        if !is_memory_datadir(&self.datadir) {
            let node_config = NodeConfigFile::new(&self.peer_table, self.node_record).await;
            store_node_config_file(node_config, self.datadir.join(NODE_CONFIG_FILENAME));
        }

        self.tracker.close();
        if tokio::time::timeout(Duration::from_secs(10), self.tracker.wait()).await.is_err() {
            warn!("Timed out waiting for node tasks to finish");
        }
        if let Err(err) = self.store.shutdown().await {
            error!("store shutdown failed: {err}");
        }
        info!("Node shutdown complete");
    }
}

/// Publishes the canonical head on a watch channel. ethrex has no head
/// subscription API, so this polls the store on a short interval.
async fn spawn_head_watcher(
    store: Store,
    blockchain: Arc<Blockchain>,
    tracker: &TaskTracker,
    cancel_token: CancellationToken,
) -> watch::Receiver<HeadInfo> {
    let initial = read_head(&store, &blockchain).await.unwrap_or_default();
    let (tx, rx) = watch::channel(initial);
    tracker.spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => break,
                _ = tokio::time::sleep(HEAD_WATCH_INTERVAL) => {}
            }
            match read_head(&store, &blockchain).await {
                Ok(head) => {
                    tx.send_if_modified(|current| {
                        let changed = *current != head;
                        *current = head;
                        changed
                    });
                }
                Err(err) => debug!("head watcher failed to read head: {err}"),
            }
        }
    });
    rx
}

async fn read_head(store: &Store, blockchain: &Blockchain) -> eyre::Result<HeadInfo> {
    let number = store.get_latest_block_number().await?;
    let hash = store
        .get_canonical_block_hash(number)
        .await?
        .ok_or_else(|| eyre::eyre!("no canonical hash for head block {number}"))?;
    let timestamp =
        store.get_block_header(number)?.map(|header| header.timestamp).unwrap_or_default();
    Ok(HeadInfo { number, hash, timestamp, is_synced: blockchain.is_synced() })
}

/// Re-apply blocks from the last on-disk state root up to the head block,
/// rebuilding the in-memory trie diff-layers lost across a restart.
async fn regenerate_head_state(store: &Store, blockchain: &Arc<Blockchain>) -> eyre::Result<()> {
    let head_block_number = store.get_latest_block_number().await?;
    let Some(last_header) = store.get_block_header(head_block_number)? else {
        eyre::bail!("database is empty, genesis block should be present");
    };

    let mut current = last_header;
    while !store.has_state_root(current.state_root)? {
        if current.number == 0 {
            eyre::bail!("unknown state found in DB; erase the datadir and resync");
        }
        let parent_number = current.number - 1;
        current = store
            .get_block_header(parent_number)?
            .ok_or_else(|| eyre::eyre!("parent header for block {parent_number} not found"))?;
    }

    if current.number == head_block_number {
        debug!("State is already up to date");
        return Ok(());
    }

    info!("Regenerating state from block {} to {head_block_number}", current.number);
    for number in (current.number + 1)..=head_block_number {
        let block = store
            .get_block_by_number(number)
            .await?
            .ok_or_else(|| eyre::eyre!("block {number} not found"))?;
        blockchain.add_block_pipeline(block, None)?;
    }
    info!("Finished regenerating state");
    Ok(())
}

pub fn is_memory_datadir(datadir: &Path) -> bool {
    datadir.ends_with("memory")
}

fn effective_datadir(base: &Path, network: &Network) -> PathBuf {
    if is_memory_datadir(base) {
        return base.to_path_buf();
    }
    match network.datadir_suffix() {
        Some(suffix) => base.join(suffix),
        None => base.to_path_buf(),
    }
}

fn init_datadir(datadir: &Path) {
    if datadir.exists() {
        assert!(datadir.is_dir(), "datadir {datadir:?} exists but is not a directory");
    } else {
        std::fs::create_dir_all(datadir).expect("failed to create data directory");
    }
}

fn get_signer(datadir: &Path) -> SecretKey {
    if is_memory_datadir(datadir) {
        return random_secret_key();
    }
    let key_path = datadir.join("node.key");
    match std::fs::read(&key_path) {
        Ok(content) => SecretKey::from_slice(&content).expect("invalid node.key"),
        Err(_) => {
            info!("Key file not found, creating a new key at {key_path:?}");
            if let Some(parent) = key_path.parent() {
                std::fs::create_dir_all(parent).expect("key file path could not be created");
            }
            let signer = random_secret_key();
            std::fs::write(key_path, signer.secret_bytes())
                .expect("newly created signer could not be saved to disk");
            signer
        }
    }
}

fn random_secret_key() -> SecretKey {
    loop {
        let bytes: [u8; 32] = rand::random();
        if let Ok(key) = SecretKey::from_slice(&bytes) {
            return key;
        }
    }
}

fn get_local_p2p_node(opts: &NodeOptions, signer: &SecretKey) -> (Node, NetworkConfig) {
    let tcp_port: u16 = opts.p2p_port.parse().expect("failed to parse p2p port");
    let udp_port: u16 = opts.discovery_port.parse().expect("failed to parse discovery port");

    let (bind_addr, external_addr) =
        resolve_p2p_endpoints(opts.p2p_addr.as_deref(), opts.nat_extip.as_deref());

    let node = Node::new(external_addr, udp_port, tcp_port, public_key_from_signing_key(signer));
    info!(enode = %node.enode_url(), "Local node initialized");
    let network_config = NetworkConfig { bind_addr, tcp_port, udp_port };
    (node, network_config)
}

/// Simplified version of upstream's endpoint resolution: no local-IP
/// auto-detection. Operators announcing to public networks should set
/// `--nat.extip` (or a specific `--p2p.addr`).
fn resolve_p2p_endpoints(p2p_addr: Option<&str>, nat_extip: Option<&str>) -> (IpAddr, IpAddr) {
    let bind: IpAddr = p2p_addr
        .map(|addr| addr.parse().expect("failed to parse --p2p.addr"))
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let external = match nat_extip {
        Some(extip) => extip.parse().expect("failed to parse --nat.extip"),
        None => {
            if bind.is_unspecified() {
                warn!(
                    "no --nat.extip and --p2p.addr is unspecified; announcing {bind} makes this \
                     node unreachable for inbound peers"
                );
            }
            bind
        }
    };
    (bind, external)
}

fn get_local_node_record(datadir: &Path, local_p2p_node: &Node, signer: &SecretKey) -> NodeRecord {
    let seq = match read_node_config_file(datadir) {
        Ok(Some(config)) => config.node_record.seq + 1,
        _ => SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
    };
    NodeRecord::from_node(local_p2p_node, seq, signer)
        .expect("node record could not be created from local node")
}

#[derive(Serialize, Deserialize)]
struct NodeConfigFile {
    known_peers: Vec<Node>,
    node_record: NodeRecord,
}

impl NodeConfigFile {
    async fn new(peer_table: &PeerTable, node_record: NodeRecord) -> Self {
        let known_peers = peer_table.get_connected_nodes().await.unwrap_or_default();
        NodeConfigFile { known_peers, node_record }
    }
}

fn read_node_config_file(datadir: &Path) -> eyre::Result<Option<NodeConfigFile>> {
    let file_path = datadir.join(NODE_CONFIG_FILENAME);
    if !file_path.exists() {
        return Ok(None);
    }
    let file = std::fs::File::open(file_path)?;
    Ok(Some(serde_json::from_reader(file)?))
}

fn store_node_config_file(config: NodeConfigFile, file_path: PathBuf) {
    match serde_json::to_string(&config) {
        Ok(json) => {
            if let Err(e) = std::fs::write(file_path, json) {
                error!("Could not store node config in file: {e:?}");
            }
        }
        Err(e) => error!("Could not serialize node config: {e:?}"),
    }
}

fn read_jwtsecret_file(jwt_secret_path: &str) -> eyre::Result<bytes::Bytes> {
    match std::fs::read_to_string(jwt_secret_path) {
        Ok(contents) => {
            let hex_str = contents.strip_prefix("0x").unwrap_or(&contents).trim_end_matches('\n');
            Ok(hex::decode(hex_str)
                .map_err(|e| eyre::eyre!("jwt secret at {jwt_secret_path} is not hex: {e}"))?
                .into())
        }
        Err(_) => {
            info!("JWT secret not found at {jwt_secret_path}, generating one");
            let secret: [u8; 32] = rand::random();
            if let Some(parent) = Path::new(jwt_secret_path).parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(jwt_secret_path, hex::encode(secret))?;
            Ok(secret.to_vec().into())
        }
    }
}

fn client_version() -> ethrex_rpc::ClientVersion {
    ethrex_rpc::ClientVersion::new(
        env!("CARGO_PKG_NAME").to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
        "helix".to_string(),
        "unknown".to_string(),
        format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        "unknown".to_string(),
    )
}

fn client_version_string() -> String {
    client_version().to_string()
}

/// Spawns a subsystem whose failure is fatal to the node: an error cancels the
/// node's token so the main loop tears everything down.
fn spawn_fatal<F, E>(
    tracker: &TaskTracker,
    cancel_token: CancellationToken,
    name: &'static str,
    fut: F,
) where
    F: std::future::Future<Output = Result<(), E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    tracker.spawn(async move {
        match fut.await {
            Ok(()) => {}
            Err(err) if cancel_token.is_cancelled() => {
                debug!("{name} returned after shutdown began: {err}");
            }
            Err(err) => {
                error!("{name} failed: {err}; shutting down the node");
                cancel_token.cancel();
            }
        }
    });
}
