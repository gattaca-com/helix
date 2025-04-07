use helix_types::{
    BlsKeypair, ChainSpec, SignedRoot, SignedValidatorRegistration, ValidatorRegistration,
};
use helix_utils::utcnow_sec;

pub fn gen_signed_vr() -> SignedValidatorRegistration {
    let keypair = BlsKeypair::random();
    let pk = keypair.pk;

    let vr = ValidatorRegistration {
        fee_recipient: Default::default(),
        gas_limit: 0,
        timestamp: utcnow_sec(),
        pubkey: pk,
    };

    let fk = ChainSpec::mainnet();
    let domain = fk.get_builder_domain();
    let root = vr.signing_root(domain);

    let sig = keypair.sk.sign(root);

    SignedValidatorRegistration { message: vr, signature: sig }
}

#[cfg(test)]
mod proposer_api_tests {
    // +++ IMPORTS +++
    use std::{sync::Arc, time::Duration};

    use alloy_primitives::{address, hex, U256};
    use helix_beacon_client::mock_multi_beacon_client::MockMultiBeaconClient;
    use helix_common::{
        api::{
            builder_api::BuilderGetValidatorsResponseEntry,
            proposer_api::ValidatorRegistrationInfo, PATH_GET_PAYLOAD, PATH_PROPOSER_API,
            PATH_REGISTER_VALIDATORS,
        },
        chain_info::ChainInfo,
        ValidatorPreferences,
    };
    use helix_database::mock_database_service::MockDatabaseService;
    use helix_datastore::MockAuctioneer;
    use helix_housekeeper::{ChainUpdate, PayloadAttributesUpdate, SlotUpdate};
    use helix_types::{
        get_fixed_pubkey, BlobsBundle, BlsPublicKey, BlsSignature, BuilderBidDeneb,
        ExecutionPayload, ExecutionPayloadDeneb, PayloadAndBlobs, SignedBlindedBeaconBlock,
        SignedBlindedBeaconBlockDeneb, SignedBuilderBid, SignedValidatorRegistration,
        TestRandomSeed, ValidatorRegistration,
    };
    use helix_utils::utcnow_ns;
    use reqwest::StatusCode;
    use serial_test::serial;
    use tokio::{
        sync::{
            mpsc::{channel, Receiver, Sender},
            oneshot,
        },
        time::sleep,
    };

    use crate::{
        gossiper::{mock_gossiper::MockGossiper, types::GossipedMessage},
        proposer::{api::ProposerApi, tests::gen_signed_vr},
        test_utils::proposer_api_app,
    };

    // +++ HELPER VARIABLES +++
    const ADDRESS: &str = "0.0.0.0";
    const PORT: u16 = 3000;
    const HEAD_SLOT: u64 = 32; //ethereum_consensus::configs::mainnet::CAPELLA_FORK_EPOCH;
    const SUBMISSION_SLOT: u64 = HEAD_SLOT + 1;
    const SUBMISSION_TIMESTAMP: u64 = 1606824419;
    const VALIDATOR_INDEX: usize = 1;
    const PARENT_HASH: &str = "0x9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e4";
    const PUB_KEY: &str = "0x84e975405f8691ad7118527ee9ee4ed2e4e8bae973f6e29aa9ca9ee4aea83605ae3536d22acc9aa1af0545064eacf82e";

    // +++ HELPER FUNCTIONS +++
    #[derive(Debug, Clone)]
    struct HttpServiceConfig {
        address: String,
        port: u16,
    }

    impl HttpServiceConfig {
        fn new(address: &str, port: u16) -> Self {
            HttpServiceConfig { address: address.to_string(), port }
        }

        fn base_url(&self) -> String {
            format!("http://{}:{}", self.address, self.port)
        }

        fn bind_address(&self) -> String {
            format!("{}:{}", self.address, self.port)
        }
    }

    fn get_valid_payload_register_validator(
        submission_slot: Option<u64>,
        validator_index: Option<usize>,
    ) -> BuilderGetValidatorsResponseEntry {
        BuilderGetValidatorsResponseEntry {
            slot: submission_slot.unwrap_or(SUBMISSION_SLOT).into(),
            validator_index: validator_index.unwrap_or(VALIDATOR_INDEX) as u64,
            entry: ValidatorRegistrationInfo {
                registration: SignedValidatorRegistration {
                    message: ValidatorRegistration {
                        fee_recipient: address!("5cc0dde14e7256340cc820415a6022a7d1c93a35"),
                        gas_limit: 30000000,
                        timestamp: SUBMISSION_TIMESTAMP,
                        pubkey: get_fixed_pubkey(Some(0)),
                    },
                    signature: BlsSignature::test_random(),
                },
                preferences: ValidatorPreferences::default(),
            },
        }
    }

    fn get_dummy_slot_update(
        head_slot: Option<u64>,
        submission_slot: Option<u64>,
        validator_index: Option<usize>,
    ) -> SlotUpdate {
        SlotUpdate {
            slot: head_slot.unwrap_or(HEAD_SLOT),
            next_duty: Some(get_valid_payload_register_validator(submission_slot, validator_index)),
            new_duties: Some(vec![get_valid_payload_register_validator(
                submission_slot,
                validator_index,
            )]),
        }
    }

    async fn send_dummy_slot_update(
        slot_update_sender: Sender<ChainUpdate>,
        head_slot: Option<u64>,
        submission_slot: Option<u64>,
        validator_index: Option<usize>,
    ) {
        let chain_update = ChainUpdate::SlotUpdate(Box::new(get_dummy_slot_update(
            head_slot,
            submission_slot,
            validator_index,
        )));
        slot_update_sender.send(chain_update).await.unwrap();

        // sleep for a bit to allow the api to process the slot update
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    async fn send_dummy_payload_attr_update(
        slot_update_sender: Sender<ChainUpdate>,
        submission_slot: u64,
    ) {
        let chain_update = ChainUpdate::PayloadAttributesUpdate(PayloadAttributesUpdate {
            slot: submission_slot,
            parent_hash: Default::default(),
            withdrawals_root: Default::default(),
            payload_attributes: Default::default(),
        });
        slot_update_sender.send(chain_update).await.unwrap();

        // sleep for a bit to allow the api to process the slot update
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    async fn start_api_server() -> (
        oneshot::Sender<()>,
        HttpServiceConfig,
        Arc<ProposerApi<MockAuctioneer, MockDatabaseService, MockMultiBeaconClient, MockGossiper>>,
        Receiver<Sender<ChainUpdate>>,
        Arc<MockAuctioneer>,
    ) {
        let (tx, rx) = oneshot::channel();
        let http_config = HttpServiceConfig::new(ADDRESS, PORT);
        let bind_address = http_config.bind_address();

        let (router, api, slot_update_receiver, auctioneer) = proposer_api_app();

        // Run the app in a background task
        tokio::spawn(async move {
            // run it with hyper on localhost:3000
            let listener = tokio::net::TcpListener::bind(bind_address).await.unwrap();
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    rx.await.ok();
                })
                .await
                .unwrap();
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        (tx, http_config, api, slot_update_receiver, auctioneer)
    }

    fn calculate_current_slot() -> u64 {
        let genesis_time_in_secs: u64 = ChainInfo::for_mainnet().genesis_time_in_secs;
        let seconds_per_slot: u64 = ChainInfo::for_mainnet().seconds_per_slot();
        let request_time_in_ns = utcnow_ns();
        let current_time_in_secs = request_time_in_ns / 1_000_000_000;
        let time_since_genesis = current_time_in_secs.saturating_sub(genesis_time_in_secs);

        time_since_genesis / seconds_per_slot
    }

    fn get_signed_builder_bid(value: U256) -> SignedBuilderBid {
        SignedBuilderBid {
            message: BuilderBidDeneb { value, ..BuilderBidDeneb::test_random() }.into(),
            signature: BlsSignature::test_random(),
        }
    }

    fn get_blinded_beacon_block(slot: u64, proposer_index: usize) -> SignedBlindedBeaconBlock {
        let mut b = SignedBlindedBeaconBlockDeneb::test_random();
        b.message.slot = slot.into();
        b.message.proposer_index = proposer_index as u64;

        b.into()
    }

    fn get_invalid_sig_signed_blinded_beacon_block(
        slot: u64,
        proposer_index: usize,
    ) -> SignedBlindedBeaconBlock {
        get_blinded_beacon_block(slot, proposer_index)
    }

    // FIXME: this is the same as invalid..
    fn get_valid_signed_blinded_beacon_block(
        slot: u64,
        proposer_index: usize,
    ) -> SignedBlindedBeaconBlock {
        get_blinded_beacon_block(slot, proposer_index)
    }

    fn load_bytes(filename: &str) -> Vec<u8> {
        use std::io::Read;

        let mut file = std::fs::File::open(filename).unwrap();
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer).unwrap();

        buffer
    }

    fn load_signed_blinded_beacon_block_from_file_fixed(
        filename: &str,
    ) -> SignedBlindedBeaconBlock {
        let mut current_dir = std::env::current_dir().expect("Failed to get current directory");
        if !current_dir.ends_with("api") {
            current_dir.push("crates/api/");
        }
        current_dir.push("test_data/");
        current_dir.push(filename);
        let req_payload_bytes =
            load_bytes(current_dir.to_str().expect("Failed to convert path to string"));
        let signed_blinded_block: SignedBlindedBeaconBlock =
            serde_json::from_slice(&req_payload_bytes).unwrap();

        signed_blinded_block
    }

    // +++ TESTS +++

    // GET_HEADER
    #[tokio::test]
    #[serial]
    async fn test_get_header_for_past_slot() {
        // Start the server
        let (tx, http_config, _api, mut slot_update_receiver, _auctioneer) =
            start_api_server().await;

        // Send slot & payload attributes updates
        let slot_update_sender = slot_update_receiver.recv().await.unwrap();
        send_dummy_slot_update(slot_update_sender.clone(), None, None, None).await;

        // Prepare the request
        let req_url = format!(
            "{}{}/header/{}/{}/{}",
            http_config.base_url(),
            PATH_PROPOSER_API,
            1,
            PARENT_HASH,
            PUB_KEY,
        );

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .get(req_url.as_str())
            .header("accept", "application/json")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.text().await.unwrap(),
            "request for past slot. request slot: 1, head slot: 32"
        );

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    async fn test_get_header_too_far_into_slot() {
        // Start the server
        let (tx, http_config, _api, mut slot_update_receiver, _auctioneer) =
            start_api_server().await;

        // Send slot & payload attributes updates
        let slot_update_sender = slot_update_receiver.recv().await.unwrap();
        send_dummy_slot_update(slot_update_sender.clone(), None, None, None).await;

        // Prepare the request
        let req_url = format!(
            "{}{}/header/{}/{}/{}",
            http_config.base_url(),
            PATH_PROPOSER_API,
            HEAD_SLOT,
            PARENT_HASH,
            PUB_KEY,
        );

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .get(req_url.as_str())
            .header("accept", "application/json")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        // we cant assert the body because, it is empty for NO_CONTENT responses

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    #[ignore]
    async fn test_get_header_for_current_slot_no_header() {
        // Start the server
        let (tx, http_config, _api, mut slot_update_receiver, _auctioneer) =
            start_api_server().await;

        // Send slot & payload attributes updates
        let slot_update_sender = slot_update_receiver.recv().await.unwrap();
        send_dummy_slot_update(slot_update_sender.clone(), None, None, None).await;

        let current_slot = calculate_current_slot();

        // Prepare the request
        let req_url = format!(
            "{}{}/header/{}/{}/{}",
            http_config.base_url(),
            PATH_PROPOSER_API,
            current_slot + 1,
            PARENT_HASH,
            PUB_KEY,
        );

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .get(req_url.as_str())
            .header("accept", "application/json")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        // we cant assert the body because, it is empty for NO_CONTENT responses

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    async fn test_get_header_for_current_slot_bid_value_zero() {
        // Start the server
        let (tx, http_config, _api, mut slot_update_receiver, auctioneer) =
            start_api_server().await;

        // Set a SignedBuilderBid in the auctioneer
        let builder_bid = get_signed_builder_bid(U256::ZERO);
        let _ = auctioneer.best_bid.lock().unwrap().insert(builder_bid.clone());

        // Send slot & payload attributes updates
        let slot_update_sender = slot_update_receiver.recv().await.unwrap();
        send_dummy_slot_update(slot_update_sender.clone(), None, None, None).await;

        let current_slot = calculate_current_slot();

        // Prepare the request
        let req_url = format!(
            "{}{}/header/{}/{}/{}",
            http_config.base_url(),
            PATH_PROPOSER_API,
            current_slot + 1,
            PARENT_HASH,
            PUB_KEY,
        );

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .get(req_url.as_str())
            .header("accept", "application/json")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        // we cant assert the body because, it is empty for NO_CONTENT responses

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    async fn test_get_header_for_current_slot_auctioneer_error() {
        // Start the server
        let (tx, http_config, _api, mut slot_update_receiver, auctioneer) =
            start_api_server().await;

        // Set a SignedBuilderBid in the auctioneer
        let builder_bid = get_signed_builder_bid(U256::from(9999)); // special value that results in an auctioneer error
        let _ = auctioneer.best_bid.lock().unwrap().insert(builder_bid.clone());

        // Send slot & payload attributes updates
        let slot_update_sender = slot_update_receiver.recv().await.unwrap();
        send_dummy_slot_update(slot_update_sender.clone(), None, None, None).await;

        let current_slot = calculate_current_slot();

        // Prepare the request
        let req_url = format!(
            "{}{}/header/{}/{}/{}",
            http_config.base_url(),
            PATH_PROPOSER_API,
            current_slot + 1,
            PARENT_HASH,
            PUB_KEY,
        );

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .get(req_url.as_str())
            .header("accept", "application/json")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(resp.text().await.unwrap(), "Internal server error");

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    async fn test_get_header_for_current_slot_ok() {
        // Start the server
        let (tx, http_config, _api, mut slot_update_receiver, auctioneer) =
            start_api_server().await;

        // Set a SignedBuilderBid in the auctioneer
        let builder_bid = get_signed_builder_bid(U256::from(10));
        let _ = auctioneer.best_bid.lock().unwrap().insert(builder_bid.clone());

        // Send slot & payload attributes updates
        let slot_update_sender = slot_update_receiver.recv().await.unwrap();
        send_dummy_slot_update(slot_update_sender.clone(), None, None, None).await;

        let current_slot = calculate_current_slot();

        // Prepare the request
        let req_url = format!(
            "{}{}/header/{}/{}/{}",
            http_config.base_url(),
            PATH_PROPOSER_API,
            current_slot + 1,
            PARENT_HASH,
            PUB_KEY,
        );

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .get(req_url.as_str())
            .header("accept", "application/json")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        // Now we can assert the body
        // and assert it can be deserialized into a SignedBuilderBid
        let body = resp.text().await.unwrap();
        let bid: SignedBuilderBid = serde_json::from_str(&body).unwrap();
        assert_eq!(bid.message.value(), builder_bid.message.value());

        // Shut down the server
        let _ = tx.send(());
    }

    // GET_PAYLOAD
    #[tokio::test]
    #[serial]
    #[ignore]
    async fn test_get_payload_no_proposer_duty() {
        // Start the server
        let (tx, http_config, _api, _slot_update_receiver, auctioneer) = start_api_server().await;

        // Set a SignedBuilderBid in the auctioneer
        let builder_bid = get_signed_builder_bid(U256::from(10));
        let _ = auctioneer.best_bid.lock().unwrap().insert(builder_bid.clone());

        let current_slot = calculate_current_slot();

        // Prepare the request
        let req_url =
            format!("{}{}{}", http_config.base_url(), PATH_PROPOSER_API, PATH_GET_PAYLOAD);

        let signed_blinded_beacon_block = get_valid_signed_blinded_beacon_block(current_slot, 1);

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .post(req_url.as_str())
            .header("accept", "*/*")
            .header("Content-Type", "application/json")
            .json(&signed_blinded_beacon_block)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(resp.text().await.unwrap(), "proposer not registered");

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    #[ignore]
    async fn test_get_payload_validator_index_mismatch() {
        // Start the server
        let (tx, http_config, _api, mut slot_update_receiver, auctioneer) =
            start_api_server().await;

        // Set a SignedBuilderBid in the auctioneer
        let builder_bid = get_signed_builder_bid(U256::from(10));
        let _ = auctioneer.best_bid.lock().unwrap().insert(builder_bid.clone());

        // Send slot & payload attributes updates
        let slot_update_sender = slot_update_receiver.recv().await.unwrap();
        send_dummy_slot_update(slot_update_sender.clone(), None, None, None).await;

        let current_slot = calculate_current_slot();

        // Prepare the request
        let req_url =
            format!("{}{}{}", http_config.base_url(), PATH_PROPOSER_API, PATH_GET_PAYLOAD);

        let signed_blinded_beacon_block = get_valid_signed_blinded_beacon_block(current_slot, 2);

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .post(req_url.as_str())
            .header("accept", "*/*")
            .header("Content-Type", "application/json")
            .json(&signed_blinded_beacon_block)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(resp.text().await.unwrap(), "unexpected proposer index. expected: 1. actual: 2");

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    async fn test_get_payload_invalid_signature() {
        // Start the server
        let (tx, http_config, _api, mut slot_update_receiver, auctioneer) =
            start_api_server().await;

        // Set a SignedBuilderBid in the auctioneer
        let builder_bid = get_signed_builder_bid(U256::from(10));
        let _ = auctioneer.best_bid.lock().unwrap().insert(builder_bid.clone());

        let current_slot = calculate_current_slot();

        // Send slot & payload attributes updates
        let slot_update_sender = slot_update_receiver.recv().await.unwrap();
        send_dummy_slot_update(
            slot_update_sender.clone(),
            Some(current_slot - 1),
            Some(current_slot),
            None,
        )
        .await;
        send_dummy_payload_attr_update(slot_update_sender.clone(), current_slot).await;

        // Prepare the request
        let req_url =
            format!("{}{}{}", http_config.base_url(), PATH_PROPOSER_API, PATH_GET_PAYLOAD);

        let signed_blinded_beacon_block =
            get_invalid_sig_signed_blinded_beacon_block(current_slot, 1);

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .post(req_url.as_str())
            .header("accept", "*/*")
            .header("Content-Type", "application/json")
            .json(&signed_blinded_beacon_block)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(resp.text().await.unwrap().starts_with("Invalid signature"));

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    #[ignore]
    async fn test_get_payload_not_found() {
        // Start the server
        let (tx, http_config, _api, mut slot_update_receiver, auctioneer) =
            start_api_server().await;

        // Set a SignedBuilderBid in the auctioneer
        let builder_bid = get_signed_builder_bid(U256::from(10));
        let _ = auctioneer.best_bid.lock().unwrap().insert(builder_bid.clone());

        // Send slot & payload attributes updates
        let slot_update_sender = slot_update_receiver.recv().await.unwrap();
        send_dummy_slot_update(slot_update_sender.clone(), None, None, None).await;

        let current_slot = calculate_current_slot();

        // Prepare the request
        let req_url =
            format!("{}{}{}", http_config.base_url(), PATH_PROPOSER_API, PATH_GET_PAYLOAD);

        let signed_blinded_beacon_block = get_valid_signed_blinded_beacon_block(current_slot, 1);

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .post(req_url.as_str())
            .header("accept", "*/*")
            .header("Content-Type", "application/json")
            .json(&signed_blinded_beacon_block)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(resp.text().await.unwrap().starts_with("No execution payload for this request"));

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    #[ignore]
    async fn test_get_payload_payload_header_mismatch() {
        // Start the server
        let (tx, http_config, _api, mut slot_update_receiver, auctioneer) =
            start_api_server().await;

        // Set a SignedBuilderBid in the auctioneer
        let builder_bid = get_signed_builder_bid(U256::from(10));
        let _ = auctioneer.best_bid.lock().unwrap().insert(builder_bid.clone());
        let _ = auctioneer.versioned_execution_payload.lock().unwrap().insert(PayloadAndBlobs {
            execution_payload: ExecutionPayloadDeneb::test_random().into(),
            blobs_bundle: BlobsBundle::test_random(),
        });

        let current_slot = calculate_current_slot();

        // Send slot & payload attributes updates
        let slot_update_sender = slot_update_receiver.recv().await.unwrap();
        send_dummy_slot_update(
            slot_update_sender.clone(),
            Some(current_slot),
            Some(current_slot + 1),
            None,
        )
        .await;

        // Prepare the request
        let req_url =
            format!("{}{}{}", http_config.base_url(), PATH_PROPOSER_API, PATH_GET_PAYLOAD);

        let mut signed_blinded_beacon_block =
            load_signed_blinded_beacon_block_from_file_fixed("signed_blinded_beacon_block.json");
        signed_blinded_beacon_block.message_deneb_mut().unwrap().slot = (current_slot + 1).into();

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .post(req_url.as_str())
            .header("accept", "*/*")
            .header("Content-Type", "application/json")
            .json(&signed_blinded_beacon_block)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(resp.text().await.unwrap().contains("does not match payload header hash"));

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    #[ignore]
    async fn test_get_payload_type_mismatch() {
        // Start the server
        let (tx, http_config, _api, mut slot_update_receiver, auctioneer) =
            start_api_server().await;

        let current_slot = calculate_current_slot();

        // Set a SignedBuilderBid in the auctioneer
        let builder_bid = get_signed_builder_bid(U256::from(10));
        let _ = auctioneer.best_bid.lock().unwrap().insert(builder_bid.clone());
        let versioned_execution_payload = PayloadAndBlobs {
            execution_payload: ExecutionPayloadDeneb::test_random().into(),
            blobs_bundle: BlobsBundle::test_random(),
        };
        let _ = auctioneer
            .versioned_execution_payload
            .lock()
            .unwrap()
            .insert(versioned_execution_payload.clone());

        // Send slot & payload attributes updates
        let slot_update_sender = slot_update_receiver.recv().await.unwrap();
        send_dummy_slot_update(
            slot_update_sender.clone(),
            Some(current_slot),
            Some(current_slot + 1),
            Some(0),
        )
        .await;

        // Prepare the request
        let req_url =
            format!("{}{}{}", http_config.base_url(), PATH_PROPOSER_API, PATH_GET_PAYLOAD);

        let signed_blinded_beacon_block = SignedBlindedBeaconBlockDeneb::test_random();

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .post(req_url.as_str())
            .header("accept", "*/*")
            .header("Content-Type", "application/json")
            .json(&signed_blinded_beacon_block)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(resp.text().await.unwrap(), "payload type mismatch");

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    #[ignore]
    async fn test_get_payload_ok() {
        // Start the server
        let (tx, http_config, _api, mut slot_update_receiver, auctioneer) =
            start_api_server().await;

        let current_slot = calculate_current_slot();

        // Set a SignedBuilderBid in the auctioneer
        let builder_bid = get_signed_builder_bid(U256::from(10));
        let _ = auctioneer.best_bid.lock().unwrap().insert(builder_bid.clone());
        let versioned_execution_payload = PayloadAndBlobs {
            execution_payload: ExecutionPayloadDeneb::test_random().into(),
            blobs_bundle: BlobsBundle::test_random(),
        };
        let _ = auctioneer
            .versioned_execution_payload
            .lock()
            .unwrap()
            .insert(versioned_execution_payload.clone());

        // Send slot & payload attributes updates
        let slot_update_sender = slot_update_receiver.recv().await.unwrap();
        send_dummy_slot_update(
            slot_update_sender.clone(),
            Some(current_slot),
            Some(current_slot + 1),
            None,
        )
        .await;

        // Prepare the request
        let req_url =
            format!("{}{}{}", http_config.base_url(), PATH_PROPOSER_API, PATH_GET_PAYLOAD);

        let mut signed_blinded_beacon_block = SignedBlindedBeaconBlockDeneb::test_random();
        signed_blinded_beacon_block.message.proposer_index = 1;
        signed_blinded_beacon_block.message.slot = (current_slot + 1).into();

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .post(req_url.as_str())
            .header("accept", "*/*")
            .header("Content-Type", "application/json")
            .json(&signed_blinded_beacon_block)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        // Now we can assert the body and assert it can be deserialized into a ExecutionPayload
        let body = resp.text().await.unwrap();
        let payload: ExecutionPayload = serde_json::from_str(&body).unwrap();
        assert_eq!(
            payload.block_hash(),
            versioned_execution_payload.execution_payload.block_hash()
        );

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    #[ignore]
    async fn test_register_validators() {
        let (tx, http_config, _api, _slot_update_receiver, _auctioneer) = start_api_server().await;
        let req_url =
            format!("{}{}{}", http_config.base_url(), PATH_PROPOSER_API, PATH_REGISTER_VALIDATORS);

        let mut signed_validator_registrations = vec![];
        for _ in 0..1_000_000 {
            signed_validator_registrations.push(gen_signed_vr());
        }

        let resp = reqwest::Client::new()
            .post(req_url.as_str())
            .header("accept", "*/*")
            .header("Content-Type", "application/json")
            .json(&signed_validator_registrations)
            .send()
            .await
            .unwrap();

        sleep(Duration::from_secs(30)).await;

        assert_eq!(resp.status(), StatusCode::OK);
        let _ = tx.send(());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn test_register_validators_with_pref_header() {
        let (tx, http_config, _api, _slot_update_receiver, _auctioneer) = start_api_server().await;
        let req_url =
            format!("{}{}{}", http_config.base_url(), PATH_PROPOSER_API, PATH_REGISTER_VALIDATORS);

        let signed_validator_registrations = vec![gen_signed_vr()];

        let resp = reqwest::Client::new()
            .post(req_url.as_str())
            .header("accept", "*/*")
            .header("Content-Type", "application/json")
            .header("x-api-key", "valid")
            .header("x-preferences", "{\"filtering\":\"regional\", \"trusted_builders\": [\"Test1\", \"Test2\"], \"header_delay\": false}")
            .json(&signed_validator_registrations)
            .send()
            .await
            .unwrap();

        sleep(Duration::from_secs(5)).await;

        assert_eq!(resp.status(), StatusCode::OK);
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    #[ignore]
    async fn test_validate_registration() {
        let (slot_update_sender, _slot_update_receiver) = channel::<Sender<ChainUpdate>>(32);
        let (_gossip_sender, gossip_receiver) = channel::<GossipedMessage>(32);
        let (v3_sender, _v3_receiver) = channel(32);
        let auctioneer = Arc::new(MockAuctioneer::default());

        let prop_api = ProposerApi::<
            MockAuctioneer,
            MockDatabaseService,
            MockMultiBeaconClient,
            MockGossiper,
        >::new(
            auctioneer.clone(),
            Arc::new(MockDatabaseService::default()),
            Arc::new(MockGossiper::new().unwrap()),
            vec![],
            Arc::new(MockMultiBeaconClient::default()),
            Arc::new(ChainInfo::for_holesky()),
            slot_update_sender.clone(),
            Arc::new(ValidatorPreferences::default()),
            gossip_receiver,
            Default::default(),
            v3_sender,
        );

        let mut x = gen_signed_vr();

        prop_api.validate_registration(&mut x).unwrap();
    }

    #[test]
    fn test_verify_signed_blinded_block_signature_from_file_deneb() {
        let req_payload_bytes =
            include_bytes!("../../test_data/signed_blinded_beacon_block_deneb.json");

        let decoded_submission: SignedBlindedBeaconBlock =
            serde_json::from_slice(req_payload_bytes.as_slice()).unwrap();

        let chain_info = ChainInfo::for_holesky();

        let pubkey = BlsPublicKey::deserialize(hex::decode("0xb74ed6ac039a55136d5493333c32ce5b2e0152e4121b5b850830383ab836e22fb5f4f8568c61f12d0646dc0eb0c6d861" ).unwrap().as_slice()).unwrap();
        assert!(decoded_submission.verify_signature(
            None,
            &pubkey,
            &chain_info.context.fork_at_epoch(222000u64.into()),
            chain_info.genesis_validators_root,
            &chain_info.context,
        ));
    }

    #[test]
    fn test_decode_signed_blinded_block_electra() {
        let mut current_dir = std::env::current_dir().expect("Failed to get current directory");
        if !current_dir.ends_with("api") {
            current_dir.push("crates/api/");
        }
        current_dir.push("test_data/signed_blinded_beacon_block_electra.json");
        let req_payload_bytes =
            load_bytes(current_dir.to_str().expect("Failed to convert path to string"));

        let decoded_submission: SignedBlindedBeaconBlock =
            serde_json::from_slice(&req_payload_bytes).unwrap();

        assert!(decoded_submission.as_electra().is_ok());
    }
}
