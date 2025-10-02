use std::time::Duration;

use alloy_primitives::Bytes;
use helix_common::{api::builder_api::InclusionList, InclusionListConfig};
use helix_types::Transaction;
use reqwest::{Client, ClientBuilder, StatusCode};
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;
use tracing::{info, warn};

/// The relay has 6 seconds to declare an inclusion list, so no point blocking longer than that.
const GET_IL_TIMEOUT: Duration = Duration::from_secs(6);
const GET_IL_RETRY_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub struct HttpInclusionListFetcher {
    http: Client,
    config: InclusionListConfig,
}

impl HttpInclusionListFetcher {
    pub fn new(config: InclusionListConfig) -> Self {
        let http = ClientBuilder::new()
            .timeout(GET_IL_TIMEOUT)
            .build()
            .expect("Failed to build HTTP client for fetching inclusion lists");

        Self { http, config }
    }

    pub async fn fetch_inclusion_list_with_retry(&self, slot: u64) -> InclusionList {
        let mut retry_interval = tokio::time::interval(GET_IL_RETRY_INTERVAL);

        info!(head_slot = slot, "Starting to fetch inclusion list for this slot");
        loop {
            retry_interval.tick().await;

            match self.fetch_inclusion_list().await {
                Ok(inclusion_list) => return inclusion_list,
                Err(err) => {
                    warn!(head_slot = slot, "Failed to fetch inclusion list for this slot {}", err)
                }
            }
        }
    }

    async fn fetch_inclusion_list(&self) -> Result<InclusionList, InclusionListError> {
        let request_payload = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "relay_inclusionList",
            "params": []
        });

        let response =
            self.http.post(self.config.node_url.as_str()).json(&request_payload).send().await?;

        let inclusion_list = match response.status() {
            StatusCode::OK => response.json::<InclusionListResponse>().await?.into(),
            status => Err(InclusionListError::HttpResponse(format!(
                "Invalid status in response from inclusion list node. Expected 200 but got {}. Headers: {:?}. Response: {:?}",
                status,
                response.headers(),
                response
            )))?,
        };

        Ok(inclusion_list)
    }
}

/// Response from inclusion list generation node, excluding all unused jsonrpc params
#[derive(Deserialize)]
struct InclusionListResponse {
    result: Vec<Bytes>,
}

impl From<InclusionListResponse> for InclusionList {
    fn from(value: InclusionListResponse) -> Self {
        let txs: Vec<Transaction> = value.result.into_iter().map(Transaction).collect();
        Self { txs: txs.into() }
    }
}

#[derive(Debug, Error)]
enum InclusionListError {
    #[error("HTTP reqwest error. {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("HTTP response error. {0}")]
    HttpResponse(String),
    #[error("Invalid inclusion list {0}")]
    Deserialization(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use helix_types::Transaction;
    use httpmock::{Method::POST, MockServer};
    use reqwest::Url;
    use serde_json::json;

    use super::*;

    fn create_test_config(url: Url) -> InclusionListConfig {
        InclusionListConfig { node_url: url }
    }

    #[tokio::test]
    async fn successful_inclusion_list_fetch() {
        let server = MockServer::start();
        let url = Url::parse(&server.url("/")).unwrap();
        let config = create_test_config(url.clone());
        let fetcher = HttpInclusionListFetcher::new(config);

        let expected_inclusion_list =
            InclusionList { txs: vec![
                Transaction("0x02f87582426801850221646a70850221646a7082520894acabf6c2d38973a5f2ebab6b5e85623db1005a4e880ddf2f839aa3d97080c080a0088ae2635655e314949dae343ac296c3fb6ac56802e1024639f9603c61e253669f2bf33fe18ce70520abc6a662794c1ef5bb310248b7b7c4acc6be93e7885d62".into()),
                Transaction("0x02f8b5824268820130850239465d16850239465d1682728a9494373a4919b3240d86ea41593d5eba789fef384880b844095ea7b30000000000000000000000005fbe74a283f7954f10aa04c2edf55578811aeb03000000000000000000000000000000000000000000000000000009184e72a000c080a0a6d0c20df1f0582c0dbf62a125fc1874868106d845c3916d666441973fb29ff0a04f4ce8879bd85510c82246de9a58a5a705dc672b6d89cc8183544f2db8649ea9".into()),
                Transaction("0x02f8b48242688193850232306c41850232306c4182739294685ce6742351ae9b618f383883d6d1e0c5a31b4b80b844095ea7b30000000000000000000000005fbe74a283f7954f10aa04c2edf55578811aeb030000000000000000000000000000000000000000000000000de0b6b3a7640000c001a0bda9b5171b2e0d3ceceebfa4de504485bd6a8fd5041251fcd2d4aed24a65ab4ca0369e2d4bcc7ef790e64195578005c8a6c3313dd0b0bc105856d0202a4e230de9".into()),
            ].into() };

        let mock = server.mock(|when, then| {
            when.method(POST).path("/");
            then.status(200).json_body(json!({
                "jsonrpc":"2.0",
                "id":1,
                "result":[
                    "0x02f87582426801850221646a70850221646a7082520894acabf6c2d38973a5f2ebab6b5e85623db1005a4e880ddf2f839aa3d97080c080a0088ae2635655e314949dae343ac296c3fb6ac56802e1024639f9603c61e253669f2bf33fe18ce70520abc6a662794c1ef5bb310248b7b7c4acc6be93e7885d62",
                    "0x02f8b5824268820130850239465d16850239465d1682728a9494373a4919b3240d86ea41593d5eba789fef384880b844095ea7b30000000000000000000000005fbe74a283f7954f10aa04c2edf55578811aeb03000000000000000000000000000000000000000000000000000009184e72a000c080a0a6d0c20df1f0582c0dbf62a125fc1874868106d845c3916d666441973fb29ff0a04f4ce8879bd85510c82246de9a58a5a705dc672b6d89cc8183544f2db8649ea9",
                    "0x02f8b48242688193850232306c41850232306c4182739294685ce6742351ae9b618f383883d6d1e0c5a31b4b80b844095ea7b30000000000000000000000005fbe74a283f7954f10aa04c2edf55578811aeb030000000000000000000000000000000000000000000000000de0b6b3a7640000c001a0bda9b5171b2e0d3ceceebfa4de504485bd6a8fd5041251fcd2d4aed24a65ab4ca0369e2d4bcc7ef790e64195578005c8a6c3313dd0b0bc105856d0202a4e230de9",
                ]
            }));
        });

        let result = fetcher.fetch_inclusion_list().await.unwrap();
        assert_eq!(result.txs.len(), expected_inclusion_list.txs.len());
        mock.assert();
    }
}
