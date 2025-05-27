use std::time::Duration;

use helix_common::{api::builder_api::InclusionList, InclusionListConfig};
use reqwest::{Client, ClientBuilder, StatusCode, Url};
use thiserror::Error;
use tracing::{info, warn};

/// The relay has 6 seconds to declare an inclusion list, so no point blocking longer than that.
const GET_IL_TIMEOUT: Duration = Duration::from_secs(6);
const GET_IL_RETRY_INTERVAL: Duration = Duration::from_millis(100);

pub struct InclusionListFetcher {
    http: Client,
    config: InclusionListConfig,
}

impl InclusionListFetcher {
    pub fn new(config: InclusionListConfig) -> Self {
        let http = ClientBuilder::new()
            .timeout(GET_IL_TIMEOUT)
            .build()
            .expect("Failed to build HTTP client for fetching inclusion lists");

        Self { http, config }
    }

    pub async fn fetch_inclusion_list_with_retry(&self, head_slot: u64) -> InclusionList {
        let mut retry_interval = tokio::time::interval(GET_IL_RETRY_INTERVAL);

        info!(head_slot = head_slot, "Starting to fetch inclusion list for this slot");
        loop {
            retry_interval.tick().await;

            match self.fetch_inclusion_list(self.config.node.clone()).await {
                Ok(inclusion_list) => return inclusion_list,
                Err(err) => warn!(
                    head_slot = head_slot,
                    "Failed to fetch inclusion list for this slot {}", err
                ),
            }
        }
    }

    async fn fetch_inclusion_list(
        &self,
        node_url: Url,
    ) -> Result<InclusionList, InclusionListError> {
        let response = self.http.get(node_url).send().await?;

        let inclusion_list = match response.status() {
            StatusCode::OK => response.json().await?,
            status => Err(InclusionListError::Http(format!(
                "Invalid status in response from inclusion list node. Expected 200 but got {}. Headers: {:?}. Response: {:?}",
                status, response.headers(), response
            )))?,
        };

        Ok(inclusion_list)
    }
}

#[derive(Debug, Error)]
enum InclusionListError {
    #[error("HTTP reqwest error. {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("HTTP error (not from reqwest). {0}")]
    Http(String),

    #[error("Invalid inclusion list {0}")]
    Deserialization(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{bytes::Bytes, Address, B256};
    use helix_common::api::builder_api::InclusionListTx;
    use httpmock::prelude::*;
    use serde_json::json;

    use super::*;

    fn create_test_config(url: Url) -> InclusionListConfig {
        InclusionListConfig { node: url, ..Default::default() }
    }

    #[tokio::test]
    async fn successful_inclusion_list_fetch() {
        let server = MockServer::start();
        let url = Url::parse(&server.url("/")).unwrap();
        let config = create_test_config(url.clone());
        let fetcher = InclusionListFetcher::new(config);

        let expected_inclusion_list = InclusionList {
            txs: vec![InclusionListTx {
                hash: B256::default(),
                nonce: 1,
                sender: Address::default(),
                gas_priority_fee: 100,
                bytes: Bytes::default(),
                wait_time: 0,
            }],
        };

        let mock = server.mock(|when, then| {
            when.method(GET).path("/");
            then.status(200).json_body(json!({
                "txs": [{
                    "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                    "nonce": 1,
                    "sender": "0x0000000000000000000000000000000000000000",
                    "gas_priority_fee": 100,
                    "bytes": "0x",
                    "wait_time": 0
                }]
            }));
        });

        let result = fetcher.fetch_inclusion_list(url).await.unwrap();
        assert_eq!(result.txs.len(), expected_inclusion_list.txs.len());
        assert_eq!(result.txs[0].nonce, expected_inclusion_list.txs[0].nonce);
        assert_eq!(result.txs[0].gas_priority_fee, expected_inclusion_list.txs[0].gas_priority_fee);
        mock.assert();
    }
}
