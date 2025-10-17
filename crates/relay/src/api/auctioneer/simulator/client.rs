use std::time::Instant;

use helix_common::{SimulatorConfig, simulator::BlockSimError};
use helix_types::ForkName;
use reqwest::{
    RequestBuilder,
    header::{HeaderMap, HeaderValue},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tracing::{debug, error};

use crate::api::auctioneer::simulator::{
    BlockMergeRequest, BlockMergeResponse, BlockSimRequest, BlockSimRpcResponse, RpcResult,
};

#[derive(Clone)]
pub struct SimulatorClient {
    pub client: reqwest::Client,
    pub config: SimulatorConfig,
    pub sim_method_v4: String,
    pub sim_method_v5: String,
    pub is_synced: bool,
    /// For certain errors we pause sims for some time to allow time for the node to recover
    // TODO: can we get these errors even if the node is reporting that it's synced?
    pub paused_until: Option<Instant>,
    /// Current number of pending tasks (validation or merging)
    pub pending: usize,
}

impl SimulatorClient {
    pub fn new(client: reqwest::Client, config: SimulatorConfig) -> Self {
        let sim_method_v4 = format!("{}_validateBuilderSubmissionV4", config.namespace);
        let sim_method_v5 = format!("{}_validateBuilderSubmissionV5", config.namespace);
        Self {
            client,
            config,
            sim_method_v4,
            sim_method_v5,
            is_synced: false,
            paused_until: None,
            pending: 0,
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.config.url
    }

    /// A lighter check to decide whether we should accept optimistic submissions
    pub fn can_simulate_light(&self) -> bool {
        self.is_synced
            && match self.paused_until {
                Some(until) => Instant::now() > until,
                None => true,
            }
    }

    pub fn can_simulate(&self) -> bool {
        self.can_simulate_light() && self.pending < self.config.max_concurrent_tasks
    }

    pub fn can_merge(&self) -> bool {
        self.can_simulate() && self.config.is_merging_simulator
    }

    pub fn sim_request_builder(&self, fork: ForkName) -> (RequestBuilder, String) {
        let mut sim_method = &self.sim_method_v4;
        if fork == ForkName::Fulu {
            sim_method = &self.sim_method_v5;
        }

        (self.client.post(&self.config.url), sim_method.clone())
    }

    pub async fn do_sim_request(
        request: &BlockSimRequest,
        is_top_bid: bool,
        sim_method: String,
        to_send: RequestBuilder,
    ) -> Result<(), BlockSimError> {
        let mut headers = HeaderMap::new();
        if is_top_bid {
            headers.insert("X-High-Priority", HeaderValue::from_static("true"));
        };

        let rpc_payload = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": sim_method,
            "params": [request]
        });

        let to_send = to_send.headers(headers).json(&rpc_payload);

        let res = match Self::rpc_request::<BlockSimRpcResponse>(to_send).await {
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
        request: &BlockMergeRequest,
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
