#[cfg(test)]
mod data_api_tests {
    // *** IMPORTS ***
    use crate::{
        relay_data::{
            DataApi, PATH_BUILDER_BIDS_RECEIVED, PATH_DATA_API, PATH_PROPOSER_PAYLOAD_DELIVERED,
            PATH_VALIDATOR_REGISTRATION,
        },
        test_utils::data_api_app,
    };
    use ethereum_consensus::{builder::SignedValidatorRegistration, primitives::BlsPublicKey};
    use helix_common::api::data_api::{
        BuilderBlocksReceivedParams, DeliveredPayloadsResponse, ProposerPayloadDeliveredParams,
        ReceivedBlocksResponse, ValidatorRegistrationParams,
    };
    use helix_database::MockDatabaseService;
    use helix_utils::request_encoding::Encoding;
    use reqwest::{Client, Response, StatusCode};
    use serial_test::serial;
    use std::{sync::Arc, time::Duration};
    use tokio::sync::oneshot;

    // +++ HELPER VARIABLES +++
    const ADDRESS: &str = "0.0.0.0";
    const PORT: u16 = 3000;
    const HEAD_SLOT: u64 = 32;

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

    async fn send_request(req_url: &str, encoding: Encoding, req_payload: Vec<u8>) -> Response {
        let client = Client::new();
        let request = client.post(req_url).header("accept", "*/*");
        let request = encoding.to_headers(request);

        request.body(req_payload).send().await.unwrap()
    }

    async fn start_api_server() -> (
        oneshot::Sender<()>,
        HttpServiceConfig,
        Arc<DataApi<MockDatabaseService>>,
        Arc<MockDatabaseService>,
    ) {
        let (tx, rx) = oneshot::channel();
        let http_config = HttpServiceConfig::new(ADDRESS, PORT);
        let bind_address = http_config.bind_address();

        let (router, api, database) = data_api_app();

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

        (tx, http_config, api, database)
    }

    fn get_test_proposer_payload_delivered_params() -> ProposerPayloadDeliveredParams {
        ProposerPayloadDeliveredParams {
            slot: Some(HEAD_SLOT),
            cursor: None,
            limit: None,
            block_hash: None,
            block_number: None,
            proposer_pubkey: None,
            builder_pubkey: None,
            order_by: None,
        }
    }

    fn get_test_builder_blocks_received_params() -> BuilderBlocksReceivedParams {
        BuilderBlocksReceivedParams {
            slot: Some(HEAD_SLOT),
            block_hash: None,
            block_number: None,
            builder_pubkey: None,
            limit: None,
        }
    }

    fn get_test_validator_registration_params() -> ValidatorRegistrationParams {
        ValidatorRegistrationParams { pubkey: BlsPublicKey::default() }
    }

    // *** TESTS ***
    #[tokio::test]
    #[serial]
    #[ignore = "TODO: to fix"]
    async fn test_payload_delivered_slot_and_cursor() {
        // Start the server
        let (tx, http_config, _api, _database) = start_api_server().await;

        // Prepare the request
        let req_url = format!(
            "{}{}{}",
            http_config.base_url(),
            PATH_DATA_API,
            PATH_PROPOSER_PAYLOAD_DELIVERED,
        );

        let mut query_params = get_test_proposer_payload_delivered_params();
        query_params.cursor = Some(HEAD_SLOT);

        // Send JSON encoded request
        // TODO: this fails with Missing request extension: Extension of type
        // `alloc::sync::Arc<moka::sync::cache::Cache<alloc::string::String,
        // alloc::vec::Vec<helix_common::api::data_api::DeliveredPayloadsResponse>>>` was not found.
        // Perhaps you forgot to add it? See `axum::Extension`.
        let resp = reqwest::Client::new()
            .get(req_url.as_str())
            .header("accept", "application/json")
            .query(&query_params)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(resp.text().await.unwrap(), "cannot specify both slot and cursor");

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    #[ignore = "TODO: to fix"]
    async fn test_payload_delivered_ok() {
        // Start the server
        let (tx, http_config, _api, _database) = start_api_server().await;

        // Prepare the request
        let req_url = format!(
            "{}{}{}",
            http_config.base_url(),
            PATH_DATA_API,
            PATH_PROPOSER_PAYLOAD_DELIVERED,
        );

        let query_params = get_test_proposer_payload_delivered_params();

        // Send JSON encoded request
        // TODO: this fails with: "Missing request extension: Extension of type
        // `alloc::sync::Arc<moka::sync::cache::Cache<alloc::string::String,
        // alloc::vec::Vec<helix_common::api::data_api::DeliveredPayloadsResponse>>>` was not found.
        // Perhaps you forgot to add it? See `axum::Extension`."
        let resp = reqwest::Client::new()
            .get(req_url.as_str())
            .header("accept", "application/json")
            .query(&query_params)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        // Deserialize the response into a Vec<DeliveredPayloadsResponse>
        let text = resp.text().await.unwrap();
        let _response: Vec<DeliveredPayloadsResponse> = serde_json::from_str(&text).unwrap();

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    #[ignore]
    async fn test_builder_bids_missing_filter() {
        // Start the server
        let (tx, http_config, _api, _database) = start_api_server().await;

        // Prepare the request
        let req_url =
            format!("{}{}{}", http_config.base_url(), PATH_DATA_API, PATH_BUILDER_BIDS_RECEIVED,);

        let mut query_params = get_test_builder_blocks_received_params();
        query_params.slot = None;

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .get(req_url.as_str())
            .header("accept", "application/json")
            .query(&query_params)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.text().await.unwrap(),
            "need to query for specific slot or block_hash or block_number or builder_pubkey"
        );

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    #[ignore]
    async fn test_builder_bids_limit_reached() {
        // Start the server
        let (tx, http_config, _api, _database) = start_api_server().await;

        // Prepare the request
        let req_url =
            format!("{}{}{}", http_config.base_url(), PATH_DATA_API, PATH_BUILDER_BIDS_RECEIVED,);

        let mut query_params = get_test_builder_blocks_received_params();
        query_params.limit = Some(501);

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .get(req_url.as_str())
            .header("accept", "application/json")
            .query(&query_params)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(resp.text().await.unwrap(), "maximum limit is 500");

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    #[ignore = "TODO: to fix"]
    async fn test_builder_bids_ok() {
        // Start the server
        let (tx, http_config, _api, _database) = start_api_server().await;

        // Prepare the request
        let req_url =
            format!("{}{}{}", http_config.base_url(), PATH_DATA_API, PATH_BUILDER_BIDS_RECEIVED,);

        let query_params = get_test_builder_blocks_received_params();

        // Send JSON encoded request
        // TODO: this fails with: "Missing request extension: Extension of type
        // `alloc::sync::Arc<moka::sync::cache::Cache<alloc::string::String,
        // alloc::vec::Vec<helix_common::api::data_api::ReceivedBlocksResponse>>>` was not found.
        // Perhaps you forgot to add it? See `axum::Extension`."
        let resp = reqwest::Client::new()
            .get(req_url.as_str())
            .header("accept", "application/json")
            .query(&query_params)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        // Deserialize the response into a Vec<ReceivedBlocksResponse>
        let text = resp.text().await.unwrap();
        let _response: Vec<ReceivedBlocksResponse> = serde_json::from_str(&text).unwrap();

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    async fn test_validator_registration() {
        // Start the server
        let (tx, http_config, _api, _database) = start_api_server().await;

        // Prepare the request
        let req_url =
            format!("{}{}{}", http_config.base_url(), PATH_DATA_API, PATH_VALIDATOR_REGISTRATION,);

        let query_params = get_test_validator_registration_params();

        // Send JSON encoded request
        let resp = reqwest::Client::new()
            .get(req_url.as_str())
            .header("accept", "application/json")
            .query(&query_params)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        // Deserialize the response into a SignedValidatorRegistration
        let text = resp.text().await.unwrap();
        let _response: SignedValidatorRegistration = serde_json::from_str(&text).unwrap();

        // Shut down the server
        let _ = tx.send(());
    }
}
