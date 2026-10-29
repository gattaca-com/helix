use alloy_primitives::{Address, U256};
use bytes::Bytes;
use helix_common::{
    SimulatorConfig,
    simulator::{BlockSimError, SszValidationResponse, TxDetail},
};
use helix_types::ForkName;
use reqwest::{
    RequestBuilder,
    header::{HeaderMap, HeaderValue},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use ssz::Decode;
use tracing::error;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct JsonRpcError {
    message: String,
}

#[derive(Debug, serde::Deserialize)]
struct BlockSimRpcResponse {
    error: Option<JsonRpcError>,
}

/// Serialized straight into the request body, so the params are never first
/// converted into a `serde_json::Value` tree.
#[derive(serde::Serialize)]
struct JsonRpcRequest<'a, T> {
    jsonrpc: &'static str,
    id: &'static str,
    method: &'a str,
    params: [&'a T; 1],
}

impl<'a, T> JsonRpcRequest<'a, T> {
    fn new(method: &'a str, params: &'a T) -> Self {
        Self { jsonrpc: "2.0", id: "1", method, params: [params] }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(untagged)]
enum RpcResult<T> {
    Ok { result: T },
    Err { error: JsonRpcError },
}

#[derive(Clone)]
pub struct SimulatorClient {
    pub client: reqwest::Client,
    pub config: SimulatorConfig,
    pub sim_method_v4: String,
    pub sim_method_v5: String,
    /// Relay-internal merged-block validation method; never reachable by an externally
    /// submitted builder block. See `helix_common::simulator::MergedJsonValidationRequest`.
    pub sim_method_merged_v5: String,
    /// If set, use SSZ binary endpoint instead of JSON-RPC for simulations
    pub ssz_url: Option<String>,
}

impl SimulatorClient {
    pub fn new(client: reqwest::Client, config: SimulatorConfig) -> Self {
        let sim_method_v4 = format!("{}_validateBuilderSubmissionV4", config.namespace);
        let sim_method_v5 = format!("{}_validateBuilderSubmissionV5", config.namespace);
        let sim_method_merged_v5 =
            format!("{}_validateMergedBuilderSubmissionV5", config.namespace);
        let ssz_url = config.ssz_url.clone();
        Self { client, config, sim_method_v4, sim_method_v5, sim_method_merged_v5, ssz_url }
    }

    pub fn endpoint(&self) -> &str {
        &self.config.url
    }

    /// `None` for a fork the SSZ validator cannot handle, mirroring
    /// [`Self::sim_request_builder`]. The two dispatch kinds have separate fork
    /// lists: an SSZ validator is a different implementation from the JSON one.
    pub fn ssz_request_builder(&self, fork: ForkName) -> Option<RequestBuilder> {
        if !matches!(fork, ForkName::Fulu | ForkName::Gloas) {
            return None;
        }
        self.ssz_url.as_ref().map(|url| self.client.post(format!("{url}/validate")))
    }

    /// Relay-internal merged-block SSZ route; see `sim_method_merged_v5`.
    pub fn ssz_merged_request_builder(&self) -> Option<RequestBuilder> {
        self.ssz_url.as_ref().map(|url| self.client.post(format!("{url}/validate_merged")))
    }

    /// Returns `None` for a fork this client has no validation RPC method for yet, rather than
    /// silently mis-routing it to a method shaped for a different fork.
    pub fn sim_request_builder(&self, fork: ForkName) -> Option<(RequestBuilder, &str)> {
        let method = match fork {
            ForkName::Fulu => &self.sim_method_v5,
            ForkName::Bellatrix | ForkName::Capella | ForkName::Deneb | ForkName::Electra => {
                &self.sim_method_v4
            }
            ForkName::Base | ForkName::Altair | ForkName::Gloas | ForkName::Heze => return None,
        };
        Some((self.client.post(&self.config.url), method))
    }

    /// Merged-block counterpart of `sim_request_builder`: only V5 applies (the network is long
    /// past the forks V4 covers), so this doesn't need a fork parameter.
    pub fn merged_sim_request_builder(&self) -> (RequestBuilder, &str) {
        (self.client.post(&self.config.url), &self.sim_method_merged_v5)
    }

    pub async fn do_json_sim_request(
        request: &impl serde::Serialize,
        is_top_bid: bool,
        sim_method: &str,
        to_send: RequestBuilder,
    ) -> Result<(), BlockSimError> {
        let mut headers = HeaderMap::new();
        if is_top_bid {
            headers.insert("X-High-Priority", HeaderValue::from_static("true"));
        }

        let res = match Self::rpc_request::<BlockSimRpcResponse>(
            to_send.headers(headers).json(&JsonRpcRequest::new(sim_method, request)),
        )
        .await
        {
            Ok(res) => res,
            Err(err) => {
                error!(%err, "failed rpc simulation");
                return Err(BlockSimError::RpcError);
            }
        };

        if let Some(error) = res.error {
            return Err(BlockSimError::BlockValidationFailed(error.message));
        }

        Ok(())
    }

    pub async fn do_sim_request(
        body: Bytes,
        is_top_bid: bool,
        to_send: RequestBuilder,
        endpoint: &str,
    ) -> Result<Vec<TxDetail>, BlockSimError> {
        let body_len = body.len();
        Self::ssz_request(to_send.body(body), is_top_bid, endpoint, body_len).await
    }

    /// Transaction detail from a `200` body. The status alone means the block
    /// is valid; the body is telemetry only, so an unreadable body loses the
    /// detail but never fails the simulation. Reth answers with an empty body.
    fn validation_tx_details(body: &[u8]) -> Vec<TxDetail> {
        if body.is_empty() {
            return Vec::new();
        }
        SszValidationResponse::from_ssz_bytes(body).map(|r| r.txs).unwrap_or_else(|err| {
            error!(?err, "corrupt ssz validation response");
            Vec::new()
        })
    }

    async fn ssz_request(
        to_send: RequestBuilder,
        is_top_bid: bool,
        endpoint: &str,
        body_len: usize,
    ) -> Result<Vec<TxDetail>, BlockSimError> {
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/octet-stream"));
        if is_top_bid {
            headers.insert("X-High-Priority", HeaderValue::from_static("true"));
        }

        let res = match to_send.headers(headers).send().await {
            Ok(r) => r,
            Err(err) => {
                error!(%err, endpoint, "failed ssz simulation");
                return Err(BlockSimError::RpcError);
            }
        };

        match res.status().as_u16() {
            200 => match res.bytes().await {
                Ok(body) => Ok(Self::validation_tx_details(&body)),
                Err(err) => {
                    error!(%err, "failed reading ssz simulation body");
                    Ok(Vec::new())
                }
            },
            400 => Err(BlockSimError::BlockValidationFailed(res.text().await.unwrap_or_default())),
            413 => {
                error!(endpoint, body_len, "ssz simulation request over the simulator body limit");
                Err(BlockSimError::PayloadTooLarge)
            }
            424 => Err(BlockSimError::HydrationMiss),
            status => {
                let body = res.text().await.unwrap_or_default();
                error!(status, body, endpoint, "ssz simulation rejected by the simulator");
                Err(BlockSimError::RpcError)
            }
        }
    }

    pub async fn balance_request(&self, address: &Address) -> Result<U256, String> {
        let rpc_payload = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "eth_getBalance",
            "params": [address.to_checksum(None), "latest"]
        });

        let to_send = self.client.post(&self.config.url).json(&rpc_payload);
        match Self::rpc_request::<RpcResult<U256>>(to_send).await.map_err(|e| e.to_string())? {
            RpcResult::Ok { result } => Ok(result),
            RpcResult::Err { error } => Err(error.message),
        }
    }

    async fn rpc_request<T: DeserializeOwned>(
        to_send: RequestBuilder,
    ) -> Result<T, reqwest::Error> {
        let res = to_send.send().await?;
        let res = res.error_for_status()?;
        res.json().await
    }

    pub async fn is_synced(&self) -> Result<bool, reqwest::Error> {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "eth_syncing",
            "params": []
        });

        let res = self.client.post(&self.config.url).json(&payload).send().await?;
        let res = res.error_for_status()?;
        let res: Value = res.json().await?;

        // {"jsonrpc":"2.0","id":0,"result":false}
        let sync_result = &res["result"];
        match sync_result {
            // Fully synced
            Value::Bool(false) => Ok(true),
            // Still syncing, or unexpected format
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod test {
    use alloy_primitives::{B256, hex::FromHex};
    use helix_common::{
        SimulatorConfig,
        api::builder_api::{InclusionListTxWithMetadata, InclusionListWithMetadata},
        simulator::{JsonValidationRequest, MergedJsonValidationRequest},
        validator_preferences::{Filtering, ValidatorPreferences},
    };
    use helix_types::{ForkName, SignedBidSubmissionFulu};
    use serde_json::{Value, json};

    use super::{JsonRpcRequest, SimulatorClient};

    fn sim_client() -> SimulatorClient {
        SimulatorClient::new(reqwest::Client::new(), SimulatorConfig {
            url: "http://localhost:8545".into(),
            namespace: "relay".into(),
            max_concurrent_tasks: 1,
            ssz_url: None,
        })
    }

    #[test]
    fn routes_fulu_to_v5() {
        let client = sim_client();
        let (_, method) = client.sim_request_builder(ForkName::Fulu).unwrap();
        assert!(method.ends_with("V5"));
    }

    #[test]
    fn routes_electra_to_v4() {
        let client = sim_client();
        let (_, method) = client.sim_request_builder(ForkName::Electra).unwrap();
        assert!(method.ends_with("V4"));
    }

    #[test]
    fn gloas_has_no_validation_method_yet() {
        assert!(sim_client().sim_request_builder(ForkName::Gloas).is_none());
    }

    fn ssz_client() -> super::SimulatorClient {
        super::SimulatorClient::new(reqwest::Client::new(), SimulatorConfig {
            url: "http://localhost:8545".into(),
            namespace: "relay".into(),
            max_concurrent_tasks: 1,
            ssz_url: Some("http://localhost:8552".into()),
        })
    }

    #[test]
    fn ssz_request_builder_routes_fulu() {
        assert!(ssz_client().ssz_request_builder(ForkName::Fulu).is_some());
    }

    #[test]
    fn ssz_request_builder_routes_gloas() {
        assert!(ssz_client().ssz_request_builder(ForkName::Gloas).is_some());
    }

    #[test]
    fn ssz_request_builder_refuses_an_unscheduled_fork() {
        assert!(
            ssz_client().ssz_request_builder(ForkName::Heze).is_none(),
            "the SSZ path short-circuited above the fork gate #517 added",
        );
    }

    /// The body must keep the field names and encodings the simulator parses.
    #[test]
    fn json_sim_body_keeps_its_wire_shape() {
        // Deserializing the fixture's blob overflows the default test stack in debug builds.
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(json_sim_body_keeps_its_wire_shape_inner)
            .unwrap()
            .join()
            .unwrap();
    }

    fn json_sim_body_keeps_its_wire_shape_inner() {
        let submission: SignedBidSubmissionFulu = serde_json::from_slice(include_bytes!(
            "../../../types/src/testdata/signed-bid-submission-fulu-2.json"
        ))
        .unwrap();
        let inclusion_list = InclusionListWithMetadata {
            txs: vec![InclusionListTxWithMetadata {
                hash: B256::repeat_byte(3),
                bytes: submission.execution_payload.transactions[0].clone(),
            }],
        };
        let root = B256::repeat_byte(9);
        let prefs = ValidatorPreferences { filtering: Filtering::Regional, ..Default::default() };
        let request = JsonValidationRequest::new(
            36_000_000,
            &submission,
            prefs.clone(),
            root,
            &inclusion_list,
        );
        fn body(request: &JsonRpcRequest<'_, impl serde::Serialize>) -> Value {
            serde_json::from_slice(&serde_json::to_vec(request).unwrap()).unwrap()
        }
        let mut params = json!({
            "registered_gas_limit": "36000000",
            "message": submission.message,
            "execution_payload": submission.execution_payload,
            "signature": submission.signature,
            "proposer_preferences": prefs,
            "blobs_bundle": submission.blobs_bundle,
            "execution_requests": submission.execution_requests,
            "parent_beacon_block_root": root,
            "inclusion_list": inclusion_list,
            "apply_blacklist": true,
        });

        assert_eq!(
            body(&JsonRpcRequest::new("relay_validateBuilderSubmissionV5", &request)),
            json!({
                "jsonrpc": "2.0",
                "id": "1",
                "method": "relay_validateBuilderSubmissionV5",
                "params": [params],
            }),
        );

        let merged = MergedJsonValidationRequest { base: request, base_payment_tx_index: 7 };
        params["base_payment_tx_index"] = json!(7);
        assert_eq!(
            body(&JsonRpcRequest::new("relay_validateMergedBuilderSubmissionV5", &merged)),
            json!({
                "jsonrpc": "2.0",
                "id": "1",
                "method": "relay_validateMergedBuilderSubmissionV5",
                "params": [params],
            }),
        );
    }

    #[tokio::test]
    async fn balance_request() {
        let sim_client = super::SimulatorClient::new(reqwest::Client::new(), SimulatorConfig {
            url: "http://54.175.81.132:8545".into(),
            namespace: "relay".into(),
            max_concurrent_tasks: 1,
            ssz_url: None,
        });
        let builder_address =
            super::Address::from_hex("0xD9d3A3f47a56a987A8119b15C994Bc126337dd27").unwrap();
        let builder_balance = sim_client.balance_request(&builder_address).await;
        println!("{builder_balance:?}");
    }
}
