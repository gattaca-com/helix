use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize},
        Arc, Mutex,
    },
    time::Duration,
};

// ++++ IMPORTS ++++
use crate::housekeeper::{Housekeeper, SLEEP_DURATION_BEFORE_REFRESHING_VALIDATORS};
use helix_common::config::PrimevConfig;

use crate::primev_service::MockPrimevService;

use helix_beacon_client::{
    mock_multi_beacon_client::MockMultiBeaconClient, MultiBeaconClientTrait,
};
use helix_common::{
    api::builder_api::BuilderGetValidatorsResponseEntry, chain_info::ChainInfo, RelayConfig,
    ValidatorSummary,
};
use helix_database::{MockDatabaseService};
use helix_datastore::MockAuctioneer;
use tokio::{sync::broadcast, task};
use ethereum_consensus::primitives::BlsPublicKey;

const HEAD_EVENT_CHANNEL_SIZE: usize = 100;

// ++++ HELPERS ++++
fn get_housekeeper() -> HelperVars {
    let subscribed_to_head_events = Arc::new(AtomicBool::new(false));
    let chan_head_events_capacity = Arc::new(AtomicUsize::new(0));
    let known_validators: Arc<Mutex<Vec<ValidatorSummary>>> = Arc::new(Mutex::new(vec![]));
    let proposer_duties: Arc<Mutex<Vec<BuilderGetValidatorsResponseEntry>>> =
        Arc::new(Mutex::new(vec![]));
    let state_validators_has_been_read: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let proposer_duties_has_been_read: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let db = MockDatabaseService::new(known_validators.clone(), proposer_duties.clone());
    let beacon_client = MockMultiBeaconClient::new(
        subscribed_to_head_events.clone(),
        chan_head_events_capacity.clone(),
        state_validators_has_been_read.clone(),
        proposer_duties_has_been_read.clone(),
    );
    let auctioneer = MockAuctioneer::new();
    let housekeeper = Housekeeper::new(
        Arc::new(db),
        beacon_client.clone(),
        auctioneer,
        MockPrimevService::new(),
        RelayConfig::default(),
        Arc::new(ChainInfo::for_mainnet()),
    );

    HelperVars {
        housekeeper,
        subscribed_to_head_events,
        chan_head_events_capacity,
        known_validators,
        proposer_duties,
        state_validators_has_been_read,
        proposer_duties_has_been_read,
        beacon_client,
    }
}

async fn start_housekeeper(
    housekeeper: Arc<Housekeeper<MockDatabaseService, MockMultiBeaconClient, MockAuctioneer, MockPrimevService>>,
    beacon_client: MockMultiBeaconClient,
) {
    let (head_event_sender, mut head_event_receiver) = broadcast::channel(HEAD_EVENT_CHANNEL_SIZE);
    beacon_client.subscribe_to_head_events(head_event_sender).await;
    task::spawn(async move {
        housekeeper.start(&mut head_event_receiver).await.unwrap();
    });
}

struct HelperVars {
    pub housekeeper: Arc<Housekeeper<MockDatabaseService, MockMultiBeaconClient, MockAuctioneer, MockPrimevService>>,
    pub subscribed_to_head_events: Arc<AtomicBool>,
    pub chan_head_events_capacity: Arc<AtomicUsize>,
    pub known_validators: Arc<Mutex<Vec<ValidatorSummary>>>,
    pub proposer_duties: Arc<Mutex<Vec<BuilderGetValidatorsResponseEntry>>>,
    pub state_validators_has_been_read: Arc<AtomicBool>,
    pub proposer_duties_has_been_read: Arc<AtomicBool>,
    pub beacon_client: MockMultiBeaconClient,
}

// ++++ TESTS ++++
#[tokio::test]
async fn test_head_event_is_processed_by_housekeeper() {
    let vars = get_housekeeper();
    start_housekeeper(vars.housekeeper.clone(), vars.beacon_client).await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    // assert that the len of the channel is correct at 1 as the
    // beacon client sends a dummy event
    assert!(vars.chan_head_events_capacity.load(std::sync::atomic::Ordering::Relaxed) == 1);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // assert that the len of the channel is correct at 0 as the
    // housekeeper has processed the dummy event
    assert!(vars.subscribed_to_head_events.load(std::sync::atomic::Ordering::Relaxed));
    assert!(vars.chan_head_events_capacity.load(std::sync::atomic::Ordering::Relaxed) == 0);
}

#[tokio::test]
async fn test_known_validators_are_set() {
    let vars = get_housekeeper();
    start_housekeeper(vars.housekeeper.clone(), vars.beacon_client).await;
    tokio::time::sleep(SLEEP_DURATION_BEFORE_REFRESHING_VALIDATORS + Duration::from_millis(100))
        .await;

    assert!(vars.known_validators.lock().unwrap().len() == 1);
    let validators = vars.known_validators.lock().unwrap();
    assert!(validators[0].balance == 1);
    assert!(validators[0].index == 1);
}

#[tokio::test]
#[ignore = "TODO: to fix"]
async fn test_proposer_duties_set() {
    let vars = get_housekeeper();
    start_housekeeper(vars.housekeeper.clone(), vars.beacon_client).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // TODO: here the value is two
    assert!(vars.proposer_duties.lock().unwrap().len() == 1);
    let proposer_duties = vars.proposer_duties.lock().unwrap();
    assert!(proposer_duties[0].validator_index == 1);
    assert!(proposer_duties[0].slot == 19);
}

#[tokio::test]
async fn test_proposer_duties_have_been_read() {
    let vars = get_housekeeper();
    start_housekeeper(vars.housekeeper.clone(), vars.beacon_client).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(vars.proposer_duties_has_been_read.load(std::sync::atomic::Ordering::Relaxed));
}

#[tokio::test]
async fn test_state_validators_have_been_read() {
    let vars = get_housekeeper();
    start_housekeeper(vars.housekeeper.clone(), vars.beacon_client).await;
    tokio::time::sleep(SLEEP_DURATION_BEFORE_REFRESHING_VALIDATORS + Duration::from_millis(100))
        .await;

    assert!(vars.state_validators_has_been_read.load(std::sync::atomic::Ordering::Relaxed));
}

#[tokio::test]
async fn test_primev_enabled_housekeeper() {
    // Create a standard helper vars
    let vars = get_housekeeper();
    
    // Create mock Primev service with test data
    let test_validator_pubkey = BlsPublicKey::try_from(vec![1; 48].as_slice()).unwrap();
    let test_builder_pubkey = BlsPublicKey::try_from(vec![2; 48].as_slice()).unwrap();
    
    let mut mock_primev = MockPrimevService::new()
        .with_validators(vec![test_validator_pubkey.clone()])
        .with_builders(vec![test_builder_pubkey.clone()]);
    
    // Create tracking for Primev operations
    let primev_operations = Arc::new(Mutex::new(Vec::new()));
    mock_primev.set_operation_tracker(primev_operations.clone());
    
    // Create a custom config with Primev enabled
    let mut config = RelayConfig::default();
    config.primev_config = Some(PrimevConfig {
        builder_url: "http://localhost:8545".to_string(),
        builder_contract: "0x1234567890123456789012345678901234567890".to_string(),
        validator_url: "http://localhost:8545".to_string(),
        validator_contract: "0x1234567890123456789012345678901234567890".to_string(),
    });
    let known_validators: Arc<Mutex<Vec<ValidatorSummary>>> = Arc::new(Mutex::new(vec![]));
    let proposer_duties: Arc<Mutex<Vec<BuilderGetValidatorsResponseEntry>>> =
        Arc::new(Mutex::new(vec![]));
    // Create a tracked mock auctioneer
    let mock_auctioneer = MockAuctioneer::new();
    let db = Arc::new(MockDatabaseService::new(known_validators.clone(), proposer_duties.clone()));

    // Create a new housekeeper with mocks
    let housekeeper = Housekeeper::new(
        Arc::clone(&db), // Use original DB to maintain tracking
        vars.beacon_client.clone(),
        mock_auctioneer,
        mock_primev,
        config,
        Arc::new(ChainInfo::for_mainnet()),
    );
    
    // Start the housekeeper
    start_housekeeper(housekeeper.clone(), vars.beacon_client).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    
    // Verify Primev operations were performed
    let ops = primev_operations.lock().unwrap();
    assert!(ops.contains(&"get_registered_primev_builders"), "Primev builders should be fetched");
    assert!(ops.contains(&"get_registered_primev_validators"), "Primev validators should be fetched");
}
