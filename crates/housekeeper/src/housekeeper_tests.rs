use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize},
        Arc, Mutex,
    },
    time::Duration,
};

// ++++ IMPORTS ++++
use crate::housekeeper::{Housekeeper, SLEEP_DURATION_BEFORE_REFRESHING_VALIDATORS};

use helix_beacon_client::{
    mock_multi_beacon_client::MockMultiBeaconClient, MultiBeaconClientTrait,
};
use helix_common::{
    api::builder_api::BuilderGetValidatorsResponseEntry, chain_info::ChainInfo, RelayConfig,
    ValidatorSummary,
};
use helix_database::MockDatabaseService;
use helix_datastore::MockAuctioneer;
use tokio::{sync::broadcast, task};

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
    housekeeper: Arc<Housekeeper<MockDatabaseService, MockMultiBeaconClient, MockAuctioneer>>,
    beacon_client: MockMultiBeaconClient,
) {
    let (head_event_sender, mut head_event_receiver) = broadcast::channel(HEAD_EVENT_CHANNEL_SIZE);
    beacon_client.subscribe_to_head_events(head_event_sender).await;
    task::spawn(async move {
        housekeeper.start(&mut head_event_receiver).await.unwrap();
    });
}

struct HelperVars {
    pub housekeeper: Arc<Housekeeper<MockDatabaseService, MockMultiBeaconClient, MockAuctioneer>>,
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
async fn test_proposer_duties_set() {
    let vars = get_housekeeper();
    start_housekeeper(vars.housekeeper.clone(), vars.beacon_client).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

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
