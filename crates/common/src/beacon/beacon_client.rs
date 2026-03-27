use std::{sync::Arc, time::Duration};

use ::ssz::Encode;
use alloy_primitives::B256;
use futures::{Stream, StreamExt};
use helix_types::{ForkName, LhConfig, VersionedSignedProposal, spec_from_config};
use reqwest::header::CONTENT_TYPE;
use reqwest_eventsource::EventSource;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::{error, warn};

use crate::{
    BeaconClientConfig, ProposerDuty, ValidatorSummary,
    beacon::{
        error::{ApiError, BeaconClientError},
        types::{ApiResult, BeaconResponse, BroadcastValidation, StateId, SyncStatus},
    },
    chain_info::ChainInfo,
};

const CONSENSUS_VERSION_HEADER: &str = "eth-consensus-version";

// Note: we noticed that beacon clients can take 5-10s to respond with the full
// validators list in some cases. The previous timeout of 5s was too short.
const BEACON_CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct BeaconClient {
    pub http: reqwest::Client,
    pub config: BeaconClientConfig,
}

impl BeaconClient {
    pub fn new(config: BeaconClientConfig) -> Self {
        let client =
            reqwest::ClientBuilder::new().timeout(BEACON_CLIENT_REQUEST_TIMEOUT).build().unwrap();
        Self { http: client, config }
    }

    pub fn endpoint(&self) -> &str {
        self.config.url.as_str()
    }

    pub async fn get<T: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, BeaconClientError> {
        let result = self.http_get(path).await?.json().await?;
        match result {
            ApiResult::Ok(result) => Ok(result),
            ApiResult::Err(err) => Err(err.into()),
        }
    }

    pub async fn http_get(&self, path: &str) -> Result<reqwest::Response, BeaconClientError> {
        let target = self.config.url.join(path)?;
        Ok(self.http.get(target).send().await?)
    }

    /// Returns a reconnecting stream of SSE events for the given topic.
    pub fn sse_stream<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        topic: &str,
    ) -> impl Stream<Item = T> + Send + 'static {
        let url = format!("{}eth/v1/events?topics={}", self.config.url, topic);
        futures::stream::unfold((), move |()| {
            let url = url.clone();
            async move {
                loop {
                    let mut es = EventSource::get(&url);
                    while let Some(event) = es.next().await {
                        match event {
                            Ok(reqwest_eventsource::Event::Message(msg)) => {
                                match serde_json::from_str::<T>(&msg.data) {
                                    Ok(data) => return Some((data, ())),
                                    Err(err) => error!(err=%err, "SSE parse error"),
                                }
                            }
                            Ok(reqwest_eventsource::Event::Open) => {}
                            Err(err) => {
                                warn!(err=%err, "SSE stream ended, reconnecting...");
                                es.close();
                                break;
                            }
                        }
                    }
                    sleep(Duration::from_millis(500)).await;
                }
            }
        })
    }

    pub async fn sync_status(&self) -> Result<SyncStatus, BeaconClientError> {
        let response: BeaconResponse<SyncStatus> = self.get("eth/v1/node/syncing").await?;
        Ok(response.data)
    }

    /// Fetch all known validators with an `active` status.
    pub async fn get_state_validators(
        &self,
        state_id: StateId,
    ) -> Result<Vec<ValidatorSummary>, BeaconClientError> {
        let endpoint = format!("eth/v1/beacon/states/{state_id}/validators?status=active,pending");
        let result: BeaconResponse<Vec<ValidatorSummary>> = self.get(&endpoint).await?;
        Ok(result.data)
    }

    pub async fn get_proposer_duties(
        &self,
        epoch: u64,
    ) -> Result<(B256, Vec<ProposerDuty>), BeaconClientError> {
        let endpoint = format!("eth/v1/validator/duties/proposer/{epoch}");
        let mut result: BeaconResponse<Vec<ProposerDuty>> = self.get(&endpoint).await?;
        let dependent_root_value = result.meta.remove("dependent_root").ok_or_else(|| {
            BeaconClientError::MissingExpectedData(
                "missing `dependent_root` in response".to_string(),
            )
        })?;
        let dependent_root: B256 = serde_json::from_value(dependent_root_value)?;
        Ok((dependent_root, result.data))
    }

    /// `publish_block` publishes the signed beacon block ssz-encoded via
    /// <https://ethereum.github.io/beacon-APIs/#/ValidatorRequiredApi/publishBlockV2>
    pub async fn publish_block(
        &self,
        block: Arc<VersionedSignedProposal>,
        broadcast_validation: Option<BroadcastValidation>,
        fork: ForkName,
    ) -> Result<u16, BeaconClientError> {
        let target = self.config.url.join("eth/v2/beacon/blocks")?;
        let body_bytes = block.as_ssz_bytes();
        let mut request = self
            .http
            .post(target)
            .body(body_bytes)
            .header(CONSENSUS_VERSION_HEADER, fork.to_string())
            .header(CONTENT_TYPE, "application/octet-stream");

        if let Some(validation) = broadcast_validation {
            request = request.query(&[("broadcast_validation", validation.to_string())]);
        }
        let response = request.send().await?;

        match response.status() {
            reqwest::StatusCode::OK => Ok(response.status().as_u16()),
            reqwest::StatusCode::ACCEPTED => {
                let code = response.status().as_u16();
                let headers = response.headers().clone();
                let body = response.text().await?;

                warn!("Block accepted but not processed: {:?} body: {:?}", headers, body);
                if body.contains("duplicate block") {
                    return Ok(200);
                }
                Ok(code)
            }
            _ => {
                let api_err = response.json::<ApiError>().await?;
                Err(BeaconClientError::Api(api_err))
            }
        }
    }

    pub async fn get_chain_info(&self) -> Result<ChainInfo, BeaconClientError> {
        let spec: BeaconResponse<LhConfig> = self.get("eth/v1/config/spec").await?;
        let spec = spec_from_config(spec.data);

        #[derive(Debug, Serialize, Deserialize)]
        struct GenesisData {
            #[serde(with = "serde_utils::quoted_u64")]
            genesis_time: u64,
            genesis_validators_root: B256,
        }

        let genesis: BeaconResponse<GenesisData> = self.get("eth/v1/beacon/genesis").await?;
        let chain_info =
            ChainInfo::new(spec, genesis.data.genesis_validators_root, genesis.data.genesis_time);

        Ok(chain_info)
    }
}

#[cfg(test)]
pub mod mock_beacon_node {
    use std::collections::HashMap;

    use httpmock::Mock;
    use serde::{Serialize, de::DeserializeOwned};

    use super::*;

    pub struct MockBeaconNode {
        server: httpmock::MockServer,
    }

    impl Default for MockBeaconNode {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MockBeaconNode {
        pub fn new() -> Self {
            Self { server: httpmock::MockServer::start() }
        }

        pub fn beacon_client(&self) -> BeaconClient {
            BeaconClient::new(BeaconClientConfig { url: self.server.base_url().parse().unwrap() })
        }

        fn mock_api<T: Serialize + DeserializeOwned>(&self, path: &str, status: u16, body: T) {
            self.server.mock(|when, then| {
                when.path(path);
                then.status(status)
                    .header("content-type", "application/json")
                    .json_body_obj(&BeaconResponse { data: body, meta: HashMap::new() });
            });
        }

        pub fn _with_state_validators(&self, state_validators: Vec<ValidatorSummary>) {
            self.mock_api(
                "eth/v1/beacon/states/head/validators?status=active,pending",
                200,
                state_validators,
            );
        }

        pub fn with_sync_status(&self, sync_status: &SyncStatus) {
            self.mock_api("/eth/v1/node/syncing", 200, sync_status.clone());
        }

        pub fn _with_sse_event(&self, topic: &str) -> Mock<'_> {
            self.server.mock(|when, then| {
                when.path("/eth/v1/events").query_param("topics", topic);
                then.status(200);
            })
        }
    }
}

#[cfg(test)]
mod beacon_client_tests {
    use mockito::Matcher;
    use url::Url;

    use super::*;

    #[tokio::test]
    async fn test_get_sync_status_ok() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", Matcher::Regex("/eth/v1/node/syncing".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data":{"is_syncing":false,"is_optimistic":false,"head_slot":"7222736","sync_distance":"1"}}"#)
            .create();

        let client =
            BeaconClient::new(BeaconClientConfig { url: Url::parse(&server.url()).unwrap() });
        let result = client.sync_status().await;

        assert!(result.is_ok());
        let sync_status = result.unwrap();

        assert_eq!(sync_status.head_slot, 7222736);
        assert_eq!(sync_status.sync_distance, 1);
        assert!(!sync_status.is_syncing);
    }

    #[tokio::test]
    async fn test_get_state_validators_ok() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", Matcher::Regex("eth/v1/beacon/states/genesis/validators\\?status=active,pending".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"execution_optimistic":false,"data":[{"index":"0","balance":"32000000000","status":"active_ongoing","validator":{"pubkey":"0x933ad9491b62059dd065b560d256d8957a8c402cc6e8d8ee7290ae11e8f7329267a8811c397529dac52ae1342ba58c95","withdrawal_credentials":"0x00f50428677c60f997aadeab24aabf7fceaef491c96a52b463ae91f95611cf71","effective_balance":"32000000000","slashed":false,"activation_eligibility_epoch":"0","activation_epoch":"0","exit_epoch":"18446744073709551615","withdrawable_epoch":"18446744073709551615"}}]}"#)
            .create();

        let client =
            BeaconClient::new(BeaconClientConfig { url: Url::parse(&server.url()).unwrap() });
        let result = client.get_state_validators(StateId::Genesis).await;

        assert!(result.is_ok());

        let validators = result.unwrap();
        assert_eq!(validators.len(), 1);
        assert_eq!(validators[0].balance, 32000000000);
    }

    #[tokio::test]
    async fn test_get_proposer_duties_ok() {
        let mut server = mockito::Server::new();
        let _m = server.mock("GET", Matcher::Regex("/eth/v1/validator/duties/proposer/225740".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"dependent_root":"0x44bff3186a234cf4fb2799c9a44dc089e33cd976a804081c652c47a8d66f11c2","execution_optimistic":false,"data":[{"pubkey":"0x926d6d7bcd4d6066d2c8a68fd2fd07f9f9eb0ac92adb3c0ddf57b63e65c078923a7cc67a17fc38cf7acb18f37795a343","validator_index":"556715","slot":"7223680"}]}"#)
            .create();

        let client =
            BeaconClient::new(BeaconClientConfig { url: Url::parse(&server.url()).unwrap() });
        let result = client.get_proposer_duties(225740).await;

        assert!(result.is_ok());
        let (node, proposer_duties) = result.unwrap();
        assert_eq!(
            format!("{node:?}"),
            "0x44bff3186a234cf4fb2799c9a44dc089e33cd976a804081c652c47a8d66f11c2"
        );
        assert_eq!(proposer_duties.len(), 1);
    }
}
