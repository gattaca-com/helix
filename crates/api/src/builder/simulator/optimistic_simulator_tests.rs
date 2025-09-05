#[cfg(test)]
mod simulator_tests {
    use std::sync::{atomic::AtomicBool, Arc};

    use alloy_primitives::{b256, U256};
    use helix_common::{
        simulator::BlockSimError, BuilderInfo, SimulatorConfig, ValidatorPreferences,
    };
    use helix_database::mock_database_service::MockDatabaseService;
    use helix_datastore::MockAuctioneer;
    use helix_types::{
        BidTrace, BlobsBundle, BlsPublicKeyBytes, BlsSignatureBytes, ExecutionPayload,
        SignedBidSubmission, SignedBidSubmissionElectra, TestRandomSeed,
    };
    use reqwest::Client;
    use serde_json::json;

    use crate::builder::{
        optimistic_simulator::OptimisticSimulator,
        rpc_simulator::{BlockSimRpcResponse, JsonRpcError},
        BlockSimRequest,
    };

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
        OptimisticSimulator::new(Arc::new(auctioneer), Arc::new(db), http, SimulatorConfig {
            url: endpoint.to_string(),
            namespace: "test".to_string(),
        })
    }

    fn get_sim_req() -> BlockSimRequest {
        let electra_exec_payload = ExecutionPayload {
            block_hash: b256!("9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5")
                .into(),
            ..ExecutionPayload::test_random()
        };

        let bid_trace = BidTrace {
            builder_pubkey: BlsPublicKeyBytes::random(),
            block_hash: b256!("9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5"),
            ..BidTrace::test_random()
        };
        let signed_bid_submission = SignedBidSubmissionElectra {
            message: bid_trace,
            execution_payload: electra_exec_payload.into(),
            signature: BlsSignatureBytes::random(),
            blobs_bundle: BlobsBundle::default().into(),
            execution_requests: Default::default(),
        };

        let signed_bid_submission = SignedBidSubmission::Electra(signed_bid_submission);

        BlockSimRequest::new(0, &signed_bid_submission, ValidatorPreferences::default(), None, None)
    }

    #[tokio::test]
    async fn test_process_request_optimistically_ok() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_body(r#"{"jsonrpc":"2.0","id":"1","result":true}"#)
            .create();

        let builder_demoted = Arc::new(AtomicBool::new(false));
        let builder_info = BuilderInfo {
            collateral: U256::from(100),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: true,
            builder_id: None,
            builder_ids: None,
            api_key: None,
        };
        let simulator = get_optimistic_simulator(
            &server.url(),
            Some(builder_info.clone()),
            builder_demoted.clone(),
        );

        let result = simulator.process_request(get_sim_req(), &builder_info, true).await;

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
        let builder_info = BuilderInfo {
            collateral: U256::from(100),
            is_optimistic: true,
            is_optimistic_for_regional_filtering: true,
            builder_id: None,
            builder_ids: None,
            api_key: None,
        };
        let simulator = get_optimistic_simulator(
            &server.url(),
            Some(builder_info.clone()),
            builder_demoted.clone(),
        );

        let result = simulator.process_request(get_sim_req(), &builder_info, true).await;

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
        let builder_info = BuilderInfo {
            collateral: U256::from(100),
            is_optimistic: false,
            is_optimistic_for_regional_filtering: true,
            builder_id: None,
            builder_ids: None,
            api_key: None,
        };
        let simulator = get_optimistic_simulator(
            &server.url(),
            Some(builder_info.clone()),
            builder_demoted.clone(),
        );

        let result = simulator.process_request(get_sim_req(), &builder_info, true).await;

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
        let builder_info = BuilderInfo {
            collateral: U256::from(100),
            is_optimistic: false,
            is_optimistic_for_regional_filtering: true,
            builder_id: None,
            builder_ids: None,
            api_key: None,
        };
        let simulator = get_optimistic_simulator(
            &server.url(),
            Some(builder_info.clone()),
            builder_demoted.clone(),
        );

        let result = simulator.process_request(get_sim_req(), &builder_info, true).await;

        // give the simulator time to process the request
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        mock.assert();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), BlockSimError::BlockValidationFailed(_)));
        assert!(!builder_demoted.load(std::sync::atomic::Ordering::Relaxed));
    }
}
