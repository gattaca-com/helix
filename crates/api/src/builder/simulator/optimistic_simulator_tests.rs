#[cfg(test)]
mod simulator_tests {
    // ++++ IMPORTS ++++
    use crate::builder::{
        optimistic_simulator::OptimisticSimulator,
        rpc_simulator::{BlockSimRpcResponse, JsonRpcError},
        traits::BlockSimulator,
        BlockSimRequest,
    };
    use alloy::hex;
    use ethereum_consensus::{
        primitives::{BlsPublicKey, BlsSignature},
        ssz::prelude::*,
        types::mainnet::ExecutionPayload,
    };
    use helix_common::{
        bid_submission::{BidTrace, SignedBidSubmission, SignedBidSubmissionCapella},
        simulator::BlockSimError,
        BuilderInfo, ValidatorPreferences,
    };
    use helix_database::MockDatabaseService;
    use helix_datastore::MockAuctioneer;
    use rand::Rng;
    use reqwest::Client;
    use serde_json::json;
    use std::sync::{atomic::AtomicBool, Arc};

    // ++++ HELPERS ++++
    fn get_optimistic_simulator(
        endpoint: &str,
        builder_info: Option<BuilderInfo>,
        builder_demoted: Arc<AtomicBool>,
    ) -> OptimisticSimulator<MockAuctioneer, MockDatabaseService> {
        let http = Client::new();
        let mut auctioneer = MockAuctioneer::new();
        auctioneer.builder_info = builder_info;
        auctioneer.builder_demoted = builder_demoted;
        let db =
            MockDatabaseService::new(Arc::new(Default::default()), Arc::new(Default::default()));
        OptimisticSimulator::new(Arc::new(auctioneer), Arc::new(db), http, endpoint.to_string())
    }

    fn get_byte_vector_32_for_hex(hex: &str) -> ByteVector<32> {
        let bytes = hex::decode(&hex[2..]).unwrap();
        ByteVector::try_from(bytes.as_ref()).unwrap()
    }

    fn get_test_pub_key_bytes(random: bool) -> [u8; 48] {
        if random {
            let mut pubkey_array = [0u8; 48];
            rand::thread_rng().fill(&mut pubkey_array[..]);
            pubkey_array
        } else {
            let pubkey_hex = "0x84e975405f8691ad7118527ee9ee4ed2e4e8bae973f6e29aa9ca9ee4aea83605ae3536d22acc9aa1af0545064eacf82e";
            let pubkey_bytes = hex::decode(&pubkey_hex[2..]).unwrap();
            let mut pubkey_array = [0u8; 48];
            pubkey_array.copy_from_slice(&pubkey_bytes);
            pubkey_array
        }
    }

    fn get_sim_req() -> BlockSimRequest {
        let capella_exec_payload = ethereum_consensus::capella::ExecutionPayload {
            block_hash: get_byte_vector_32_for_hex(
                "0x9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5",
            ),
            ..Default::default()
        };
        let execution_payload = ExecutionPayload::Capella(capella_exec_payload);
        let bid_trace = BidTrace {
            builder_public_key: BlsPublicKey::try_from(&get_test_pub_key_bytes(false)[..]).unwrap(),
            block_hash: get_byte_vector_32_for_hex(
                "0x9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5",
            ),
            ..Default::default()
        };
        let signed_bid_submission = SignedBidSubmission::Capella(SignedBidSubmissionCapella {
            message: bid_trace,
            execution_payload,
            signature: BlsSignature::default(),
        });

        BlockSimRequest::new(
            0,
            Arc::new(signed_bid_submission),
            ValidatorPreferences::default(),
            None,
        )
    }

    // ++++ TESTS ++++
    #[tokio::test]
    async fn test_process_request_optimistically_ok() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_body(r#"{"jsonrpc":"2.0","id":"1","result":true}"#)
            .create();

        let builder_demoted = Arc::new(AtomicBool::new(false));
        let (sim_res_sender, _sim_res_receiver) = tokio::sync::mpsc::channel(100);
        let builder_info = BuilderInfo {
            collateral: U256::from(100),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: true,
            builder_id: None,
            builder_ids: None,
        };
        let simulator = get_optimistic_simulator(
            &server.url(),
            Some(builder_info.clone()),
            builder_demoted.clone(),
        );

        let result =
            simulator.process_request(get_sim_req(), &builder_info, true, sim_res_sender).await;

        // give the simulator time to process the request
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        mock.assert();
        assert!(result.is_ok());
        assert!(!builder_demoted.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_process_request_optimistically_builder_demoted() {
        let rpc_response = BlockSimRpcResponse {
            error: Some(JsonRpcError { message: "validation failed".to_string() }),
        };
        let rpc_response_json = json!(rpc_response).to_string();
        let mut server = mockito::Server::new();
        let mock = server.mock("POST", "/").with_status(200).with_body(rpc_response_json).create();

        let builder_demoted = Arc::new(AtomicBool::new(false));
        let (sim_res_sender, _sim_res_receiver) = tokio::sync::mpsc::channel(100);
        let builder_info = BuilderInfo {
            collateral: U256::from(100),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: true,
            builder_id: None,
            builder_ids: None,
        };
        let simulator = get_optimistic_simulator(
            &server.url(),
            Some(builder_info.clone()),
            builder_demoted.clone(),
        );

        let result =
            simulator.process_request(get_sim_req(), &builder_info, true, sim_res_sender).await;

        // give the simulator time to process the request
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        mock.assert();
        assert!(result.is_ok());
        assert!(builder_demoted.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_process_request_non_optimistically_ok() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_body(r#"{"jsonrpc":"2.0","id":"1","result":true}"#)
            .create();

        let builder_demoted = Arc::new(AtomicBool::new(false));
        let (sim_res_sender, _sim_res_receiver) = tokio::sync::mpsc::channel(100);
        let builder_info = BuilderInfo {
            collateral: U256::from(100),
            is_optimistic: false,
            is_optimistic_for_regional_filtering: true,
            builder_id: None,
            builder_ids: None,
        };
        let simulator = get_optimistic_simulator(
            &server.url(),
            Some(builder_info.clone()),
            builder_demoted.clone(),
        );

        let result =
            simulator.process_request(get_sim_req(), &builder_info, true, sim_res_sender).await;

        // give the simulator time to process the request
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        mock.assert();
        assert!(result.is_ok());
        assert!(!builder_demoted.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_process_request_non_optimistically_validation_failed() {
        let rpc_response = BlockSimRpcResponse {
            error: Some(JsonRpcError { message: "validation failed".to_string() }),
        };
        let rpc_response_json = json!(rpc_response).to_string();
        let mut server = mockito::Server::new();
        let mock = server.mock("POST", "/").with_status(200).with_body(rpc_response_json).create();

        let builder_demoted = Arc::new(AtomicBool::new(false));
        let (sim_res_sender, _sim_res_receiver) = tokio::sync::mpsc::channel(100);
        let builder_info = BuilderInfo {
            collateral: U256::from(100),
            is_optimistic: false,
            is_optimistic_for_regional_filtering: true,
            builder_id: None,
            builder_ids: None,
        };
        let simulator = get_optimistic_simulator(
            &server.url(),
            Some(builder_info.clone()),
            builder_demoted.clone(),
        );

        let result =
            simulator.process_request(get_sim_req(), &builder_info, true, sim_res_sender).await;

        // give the simulator time to process the request
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        mock.assert();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), BlockSimError::BlockValidationFailed(_)));
        assert!(!builder_demoted.load(std::sync::atomic::Ordering::Relaxed));
    }
}
