use alloy_primitives::{Address, U256};
use helix_common::{
    SimulatorConfig,
    simulator::{BlockSimError, JsonValidationRequest, SszValidationRequest},
};
use helix_types::ForkName;
use reqwest::{
    RequestBuilder,
    header::{HeaderMap, HeaderValue},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use ssz::Encode;
use tracing::{debug, error};

use crate::simulator::{BlockMergeResponse, MergeRequest};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct JsonRpcError {
    message: String,
}

#[derive(Debug, serde::Deserialize)]
struct BlockSimRpcResponse {
    error: Option<JsonRpcError>,
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
    /// If set, use SSZ binary endpoint instead of JSON-RPC for simulations
    pub ssz_url: Option<String>,
}

impl SimulatorClient {
    pub fn new(client: reqwest::Client, config: SimulatorConfig) -> Self {
        let sim_method_v4 = format!("{}_validateBuilderSubmissionV4", config.namespace);
        let sim_method_v5 = format!("{}_validateBuilderSubmissionV5", config.namespace);
        let ssz_url = config.ssz_url.clone();
        Self { client, config, sim_method_v4, sim_method_v5, ssz_url }
    }

    pub fn endpoint(&self) -> &str {
        &self.config.url
    }

    pub fn ssz_request_builder(&self) -> Option<RequestBuilder> {
        self.ssz_url.as_ref().map(|url| self.client.post(format!("{url}/validate")))
    }

    pub fn sim_request_builder(&self, fork: ForkName) -> (RequestBuilder, &str) {
        let method = if fork == ForkName::Fulu { &self.sim_method_v5 } else { &self.sim_method_v4 };
        (self.client.post(&self.config.url), method)
    }

    pub async fn do_json_sim_request(
        request: &JsonValidationRequest,
        is_top_bid: bool,
        sim_method: &str,
        to_send: RequestBuilder,
    ) -> Result<(), BlockSimError> {
        let mut headers = HeaderMap::new();
        if is_top_bid {
            headers.insert("X-High-Priority", HeaderValue::from_static("true"));
        }

        let rpc_payload = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": sim_method,
            "params": [request]
        });

        let res = match Self::rpc_request::<BlockSimRpcResponse>(
            to_send.headers(headers).json(&rpc_payload),
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
        ssz_req: &SszValidationRequest,
        is_top_bid: bool,
        to_send: RequestBuilder,
    ) -> Result<(), BlockSimError> {
        Self::ssz_request(to_send.body(ssz_req.as_ssz_bytes()), is_top_bid).await
    }

    async fn ssz_request(to_send: RequestBuilder, is_top_bid: bool) -> Result<(), BlockSimError> {
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/octet-stream"));
        if is_top_bid {
            headers.insert("X-High-Priority", HeaderValue::from_static("true"));
        }

        let res = match to_send.headers(headers).send().await {
            Ok(r) => r,
            Err(err) => {
                error!(%err, "failed ssz simulation");
                return Err(BlockSimError::RpcError);
            }
        };

        match res.status().as_u16() {
            200 => Ok(()),
            400 => Err(BlockSimError::BlockValidationFailed(res.text().await.unwrap_or_default())),
            424 => Err(BlockSimError::HydrationMiss),
            _ => Err(BlockSimError::RpcError),
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

    pub fn merge_request_builder(&self) -> RequestBuilder {
        self.client.post(&self.config.url)
    }

    pub async fn do_merge_request(
        request: &MergeRequest,
        to_send: RequestBuilder,
    ) -> Result<BlockMergeResponse, BlockSimError> {
        let rpc_payload = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "relay_mergeBlockV1",
            "params": [request.request]
        });

        let to_send = to_send.json(&rpc_payload);

        let res = match Self::rpc_request::<RpcResult<BlockMergeResponse>>(to_send).await {
            Ok(res) => res,
            Err(err) => {
                error!(%err, "failed rpc simulation");
                return Err(BlockSimError::RpcError);
            }
        };

        debug!(?res, "received merge response");

        match res {
            RpcResult::Err { error } => Err(BlockSimError::BlockValidationFailed(error.message)),
            RpcResult::Ok { result } => Ok(result),
        }
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
    use alloy_primitives::hex::FromHex;
    use helix_common::SimulatorConfig;

    #[tokio::test]
    async fn balance_request() {
        let sim_client = super::SimulatorClient::new(
            reqwest::Client::new(),
            SimulatorConfig {
                url: "http://54.175.81.132:8545".into(),
                namespace: "relay".into(),
                is_merging_simulator: false,
                max_concurrent_tasks: 1,
                ssz_url: None,
            },
        );
        let builder_address =
            super::Address::from_hex("0xD9d3A3f47a56a987A8119b15C994Bc126337dd27").unwrap();
        let builder_balance = sim_client.balance_request(&builder_address).await;
        println!("{builder_balance:?}");
    }
}
