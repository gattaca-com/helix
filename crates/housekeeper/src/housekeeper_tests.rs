use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize},
        Arc, Mutex,
    },
    time::Duration,
};

use helix_beacon::{
    beacon_client::mock_beacon_node::MockBeaconNode, multi_beacon_client::MultiBeaconClient,
};
use helix_common::{
    api::builder_api::BuilderGetValidatorsResponseEntry, chain_info::ChainInfo,
    config::PrimevConfig, local_cache::LocalCache, RelayConfig, ValidatorSummary,
};
use helix_database::mock_database_service::MockDatabaseService;
use helix_network::RelayNetworkApi;
use tokio::sync::broadcast;

use crate::housekeeper::Housekeeper;

fn get_housekeeper() -> HelperVars {
    let chan_head_events_capacity = Arc::new(AtomicUsize::new(0));
    let known_validators: Arc<Mutex<Vec<ValidatorSummary>>> = Arc::new(Mutex::new(vec![]));
    let proposer_duties: Arc<Mutex<Vec<BuilderGetValidatorsResponseEntry>>> =
        Arc::new(Mutex::new(vec![]));
    let state_validators_has_been_read: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let proposer_duties_has_been_read: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let db = MockDatabaseService::new(known_validators.clone(), proposer_duties.clone());
    let beacon_node = MockBeaconNode::new();
    let beacon_client = MultiBeaconClient::new(vec![beacon_node.beacon_client().into()]);
    // let beacon_client = MultiBeaconClient::new(
    //     subscribed_to_head_events.clone(),
    //     chan_head_events_capacity.clone(),
    //     state_validators_has_been_read.clone(),
    //     proposer_duties_has_been_read.clone(),
    // );

    let sorter_tx = crossbeam_channel::unbounded().0;
    let auctioneer = LocalCache::new(sorter_tx);
    let housekeeper = Housekeeper::new(
        Arc::new(db),
        beacon_client.clone().into(),
        Arc::new(auctioneer),
        &RelayConfig::default(),
        Arc::new(ChainInfo::for_mainnet()),
        RelayNetworkApi::new(Default::default(), Default::default()),
    );

    HelperVars {
        beacon_node,
        housekeeper,
        chan_head_events_capacity,
        known_validators,
        proposer_duties,
        state_validators_has_been_read,
        proposer_duties_has_been_read,
        beacon_client,
    }
}

async fn start_housekeeper(
    housekeeper: Housekeeper<MockDatabaseService>,
    beacon_client: MultiBeaconClient,
) {
    let (head_event_sender, head_event_receiver) = broadcast::channel(1);
    beacon_client.subscribe_to_head_events(head_event_sender).await;
    housekeeper.start(head_event_receiver).await.unwrap();
}

struct HelperVars {
    pub housekeeper: Housekeeper<MockDatabaseService>,
    pub chan_head_events_capacity: Arc<AtomicUsize>,
    pub known_validators: Arc<Mutex<Vec<ValidatorSummary>>>,
    pub proposer_duties: Arc<Mutex<Vec<BuilderGetValidatorsResponseEntry>>>,
    pub state_validators_has_been_read: Arc<AtomicBool>,
    pub proposer_duties_has_been_read: Arc<AtomicBool>,
    pub beacon_client: MultiBeaconClient,
    pub beacon_node: MockBeaconNode,
}

#[tokio::test]
async fn test_head_event_is_processed_by_housekeeper() {
    let vars = get_housekeeper();
    let subscribed_to_head_events = vars.beacon_node.with_sse_event("head");
    start_housekeeper(vars.housekeeper.clone(), vars.beacon_client).await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    // assert that the len of the channel is correct at 1 as the
    // beacon client sends a dummy event
    assert!(vars.chan_head_events_capacity.load(std::sync::atomic::Ordering::Relaxed) == 1);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // assert that the len of the channel is correct at 0 as the
    // housekeeper has processed the dummy event
    subscribed_to_head_events.assert();
    assert!(vars.chan_head_events_capacity.load(std::sync::atomic::Ordering::Relaxed) == 0);
}

#[tokio::test]
async fn test_known_validators_are_set() {
    let vars = get_housekeeper();
    start_housekeeper(vars.housekeeper.clone(), vars.beacon_client).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

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

    assert_eq!(vars.proposer_duties.lock().unwrap().len(), 2);
    let proposer_duties = vars.proposer_duties.lock().unwrap();
    assert_eq!(proposer_duties[0].validator_index, 1);
    assert_eq!(proposer_duties[0].slot, 19);
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
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(vars.state_validators_has_been_read.load(std::sync::atomic::Ordering::Relaxed));
}

// #[tokio::test]
// async fn test_primev_enabled_housekeeper() {
//     // Create a standard helper vars
//     let vars = get_housekeeper();

//     // Create mock Primev service with test data
//     let test_validator_pubkey = get_fixed_pubkey(0);
//     let test_builder_pubkey = get_fixed_pubkey(1);

//     let mut mock_primev = MockPrimevService::new()
//         .with_validators(vec![test_validator_pubkey.clone()])
//         .with_builders(vec![test_builder_pubkey.clone()]);

//     // Create tracking for Primev operations
//     let primev_operations = Arc::new(Mutex::new(Vec::new()));
//     mock_primev.set_operation_tracker(primev_operations.clone());

//     // Create a custom config with Primev enabled
//     let mut config = RelayConfig::default();
//     config.primev_config = Some(PrimevConfig {
//         builder_url: "http://localhost:8545".to_string(),
//         builder_contract: "0x1234567890123456789012345678901234567890".to_string(),
//         validator_url: "http://localhost:8545".to_string(),
//         validator_contract: "0x1234567890123456789012345678901234567890".to_string(),
//     });
//     let known_validators: Arc<Mutex<Vec<ValidatorSummary>>> = Arc::new(Mutex::new(vec![]));
//     let proposer_duties: Arc<Mutex<Vec<BuilderGetValidatorsResponseEntry>>> =
//         Arc::new(Mutex::new(vec![]));
//     // Create a tracked mock auctioneer
//     let mock_auctioneer = LocalCache::new_test();
//     let db = Arc::new(MockDatabaseService::new(known_validators.clone(),
// proposer_duties.clone()));

//     // Create a new housekeeper with mocks
//     let housekeeper = Housekeeper::new(
//         Arc::clone(&db), // Use original DB to maintain tracking
//         vars.beacon_client.clone().into(),
//         mock_auctioneer,
//         Some(mock_primev),
//         config,
//         Arc::new(ChainInfo::for_mainnet()),
//     );

//     // Start the housekeeper
//     start_housekeeper(housekeeper.clone(), vars.beacon_client).await;
//     tokio::time::sleep(Duration::from_millis(200)).await;

//     // Verify Primev operations were performed
//     let ops = primev_operations.lock().unwrap();
//     assert!(ops.contains(&"get_registered_primev_builders"), "Primev builders should be
// fetched");     assert!(
//         ops.contains(&"get_registered_primev_validators"),
//         "Primev validators should be fetched"
//     );
// }

// #[tokio::test]
// async fn test_primev_builder_fetch() {
//     // Create a custom config with Primev enabled - using the same URL and contract address as
// the     // working cast command
//     let config = PrimevConfig {
//         builder_url: "https://chainrpc.mev-commit.xyz/".to_string(),
//         builder_contract: "0xb772Add4718E5BD6Fe57Fb486A6f7f008E52167E".to_string(),
//         validator_url: "https://127.0.0.1:8545".to_string(),
//         validator_contract: "0x0000000000000000000000000000000000000000".to_string(),
//     };

//     let service = EthereumPrimevService::new(config.clone()).await.unwrap();

//     // Use the trait to call the method
//     let primev_service: &dyn PrimevService = &service;
//     let pubkeys = primev_service.get_registered_primev_builders().await;

//     assert!(pubkeys.len() > 0, "Expected at least one public key to be returned");
// }

#[tokio::test]
async fn test_primev_real_contract_integration() {
    use ethers::{
        abi::{Abi, Address, Token},
        contract::Contract,
        providers::{Http, Middleware, Provider},
        types::{transaction::eip2718::TypedTransaction, TransactionRequest},
    };
    // Test is ignored by default since it requires external network connectivity
    // Run with: cargo test test_primev_real_contract_integration -- --ignored
    if std::env::var("RUN_EXTERNAL_TESTS").is_err() {
        println!("Skipping external contract test. Set RUN_EXTERNAL_TESTS=1 to run.");
        return;
    }

    // Create a real EthereumPrimevService
    let config = PrimevConfig {
        builder_url: "https://eth.llamarpc.com".to_string(),
        builder_contract: "0x821798d7b9d57dF7Ed7616ef9111A616aB19ed64".to_string(),
        validator_url: "https://eth.llamarpc.com".to_string(),
        validator_contract: "0x821798d7b9d57dF7Ed7616ef9111A616aB19ed64".to_string(),
    };

    // Define a minimal version of EthereumPrimevService that uses the low-level approach
    struct TestPrimevService {
        validator_contract: Contract<Provider<Http>>,
        validator_provider: Arc<Provider<Http>>,
    }

    impl TestPrimevService {
        async fn new(config: PrimevConfig) -> Self {
            // Initialize validator contract
            let validator_provider =
                Provider::<Http>::try_from(config.validator_url.as_str()).unwrap();
            let validator_provider = Arc::new(validator_provider);
            let validator_address: Address = config.validator_contract.as_str().parse().unwrap();

            // Use the simplified ABI that we know works
            let validator_abi_str = r#"[{
                    "inputs": [{"name":"valBLSPubKeys","type":"bytes[]"}],
                    "name": "areValidatorsOptedIn", 
                    "outputs": [{"components":[
                        {"name":"isVanillaOptedIn","type":"bool"},
                        {"name":"isAvsOptedIn","type":"bool"},
                        {"name":"isMiddlewareOptedIn","type":"bool"}
                    ],"name":"","type":"tuple[]"}],
                    "stateMutability": "view",
                    "type": "function"
                }]"#;

            let validator_abi: Abi = serde_json::from_str(validator_abi_str).unwrap();
            let validator_contract =
                Contract::new(validator_address, validator_abi, Arc::clone(&validator_provider));

            Self { validator_contract, validator_provider }
        }

        // Implement the low-level contract call approach that worked in the test
        async fn get_validators_opted_in(
            &self,
            validator_pubkeys: Vec<Vec<u8>>,
        ) -> Result<Vec<(bool, bool, bool)>, Box<dyn std::error::Error + Send + Sync>> {
            // Get the function from the ABI
            let func = self.validator_contract.abi().function("areValidatorsOptedIn").unwrap();

            // Create the input token using the verified approach
            let input_tokens = vec![Token::Array(
                validator_pubkeys.iter().map(|key| Token::Bytes(key.clone())).collect(),
            )];

            // Encode the function call
            let call_data = func.encode_input(&input_tokens).unwrap();

            // Create and execute the transaction
            let tx: TypedTransaction = TransactionRequest::new()
                .to(self.validator_contract.address())
                .data(call_data)
                .into();

            let result = self.validator_provider.call(&tx, None).await?;

            // Decode the result
            let decoded = func.decode_output(&result)?;

            // Process the decoded output to extract the tuple values
            let mut statuses = Vec::new();

            if let Some(Token::Array(tuples)) = decoded.first() {
                for token in tuples {
                    if let Token::Tuple(values) = token {
                        if values.len() >= 3 {
                            if let (Token::Bool(a), Token::Bool(b), Token::Bool(c)) = (
                                values.first().unwrap_or(&Token::Bool(false)),
                                values.get(1).unwrap_or(&Token::Bool(false)),
                                values.get(2).unwrap_or(&Token::Bool(false)),
                            ) {
                                statuses.push((*a, *b, *c));
                                continue;
                            }
                        }
                    }
                    // Default if parsing fails
                    statuses.push((false, false, false));
                }
            }

            Ok(statuses)
        }
    }

    // Create our test service
    let service = TestPrimevService::new(config).await;

    // Create a test validator public key
    let test_key = vec![1u8; 48];
    let validator_pubkeys = vec![test_key];

    // Make the call
    match service.get_validators_opted_in(validator_pubkeys).await {
        Ok(statuses) => {
            println!("Successfully called contract!");
            println!("Got {} validator statuses", statuses.len());
            for (i, status) in statuses.iter().enumerate() {
                println!(
                    "Validator {}: vanilla={}, avs={}, middleware={}",
                    i, status.0, status.1, status.2
                );
            }
        }
        Err(e) => {
            eprintln!("Contract call failed: {e:?}");
            panic!("Contract call failed: {e:?}");
        }
    }
}
