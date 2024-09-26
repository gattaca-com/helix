use std::time::{SystemTime, UNIX_EPOCH};

use ethereum_consensus::{
    builder::{compute_builder_domain, SignedValidatorRegistration, ValidatorRegistration},
    crypto::SecretKey,
    signing::compute_signing_root,
};
use helix_common::chain_info::ChainInfo;
use rand::thread_rng;

pub fn gen_signed_vr() -> SignedValidatorRegistration {
    let mut rng = thread_rng();
    let sk = SecretKey::random(&mut rng).unwrap();
    let pk = sk.public_key();

    let mut vr = ValidatorRegistration {
        fee_recipient: Default::default(),
        gas_limit: 0,
        timestamp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        public_key: pk,
    };

    let fk = ChainInfo::for_mainnet();
    let domain = compute_builder_domain(&fk.context).unwrap();
    let csr = compute_signing_root(&mut vr, domain).unwrap();

    let sig = sk.sign(csr.as_ref());

    SignedValidatorRegistration { message: vr, signature: sig }
}

#[cfg(test)]
mod constraint_api_tests {
    // +++ IMPORTS +++
    use std::{sync::Arc, time::Duration};

    use helix_datastore::MockAuctioneer;
    use helix_housekeeper::ChainUpdate;
    use helix_utils::request_encoding::Encoding;
    use reqwest::{Client, Response};
    use serial_test::serial;
    use tokio::sync::{
        mpsc::{Receiver, Sender},
        oneshot,
    };

    use crate::{
        constraints::{api::ConstraintsApi, types::PATH_CONSTRAINTS_API},
        test_utils::constraints_api_app,
    };

    // +++ HELPER VARIABLES +++
    const ADDRESS: &str = "0.0.0.0";
    const PORT: u16 = 3000;

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

    async fn _send_request(req_url: &str, encoding: Encoding, req_payload: Vec<u8>) -> Response {
        let client = Client::new();
        let request = client.post(req_url).header("accept", "*/*");
        let request = encoding.to_headers(request);

        request.body(req_payload).send().await.unwrap()
    }

    async fn start_api_server(
    ) -> (oneshot::Sender<()>, HttpServiceConfig, Arc<ConstraintsApi<MockAuctioneer>>, Receiver<Sender<ChainUpdate>>, Arc<MockAuctioneer>) {
        let (tx, rx) = oneshot::channel();
        let http_config = HttpServiceConfig::new(ADDRESS, PORT);
        let bind_address = http_config.bind_address();

        let (router, api, slot_update_receiver, auctioneer) = constraints_api_app();

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

    // +++ TESTS +++

    #[tokio::test]
    #[serial]
    async fn test_get_preconfer_with_slot() {
        // Start the server
        let (tx, http_config, _api, mut _slot_update_receiver, _auctioneer) = start_api_server().await;

        // Prepare the request
        let req_url = format!("{}{}/preconfer/{}", http_config.base_url(), PATH_CONSTRAINTS_API, 1,);

        // Send JSON encoded request
        let resp = reqwest::Client::new().get(req_url.as_str()).header("accept", "application/json").send().await.unwrap();

        println!("{:?}", resp);

        // Shut down the server
        let _ = tx.send(());
    }

    #[tokio::test]
    #[serial]
    async fn test_get_preconfers_for_epoch() {
        // Start the server
        let (tx, http_config, _api, mut _slot_update_receiver, _auctioneer) = start_api_server().await;

        // Prepare the request
        let req_url = format!("{}{}/preconfers", http_config.base_url(), PATH_CONSTRAINTS_API,);

        // Send JSON encoded request
        let resp = reqwest::Client::new().get(req_url.as_str()).header("accept", "application/json").send().await.unwrap();

        // print body
        println!("status: {:?}", resp.status());
        let body = resp.text().await.unwrap();
        println!("{:?}", body);

        // Shut down the server
        let _ = tx.send(());
    }
}
