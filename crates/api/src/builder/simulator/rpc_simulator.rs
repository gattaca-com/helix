use std::sync::Arc;

use alloy_primitives::{B256, U256};
use helix_common::{
    metrics::SimulatorMetrics, simulator::BlockSimError, task, BuilderInfo, SimulatorConfig,
};
use helix_database::DatabaseService;
use helix_types::{ExecutionPayload, ExecutionRequests};
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client, Response, StatusCode,
};
use serde_json::{json, Value};
use tracing::{debug, error, Instrument};

use crate::builder::{BlockMergeRequest, BlockSimRequest};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcError {
    pub message: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct BlockSimRpcResponse {
    pub error: Option<JsonRpcError>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(untagged)]
pub enum RpcResult<T> {
    Ok(T),
    Err { error: JsonRpcError },
}

impl<T> RpcResult<T> {
    pub fn as_error(&self) -> Option<&JsonRpcError> {
        match self {
            RpcResult::Err { error } => Some(error),
            _ => None,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct BlockMergeResponse {
    pub execution_payload: ExecutionPayload,
    pub execution_requests: ExecutionRequests,
    /// Versioned hashes of the appended blob transactions.
    pub appended_blobs: Vec<B256>,
    /// Total value for the proposer
    pub proposer_value: U256,
}

/// RpcSimulator is responsible for sending block requests to the RPC endpoint for validation.
/// It uses the `flashbots_validateBuilderSubmissionV2` method for the actual validation.
#[derive(Clone)]
pub struct RpcSimulator<DB: DatabaseService + 'static> {
    http: Client,
    pub simulator_config: SimulatorConfig,
    db: Arc<DB>,
    sim_method: String,
}

impl<DB: DatabaseService + 'static> RpcSimulator<DB> {
    pub fn new(http: Client, simulator_config: SimulatorConfig, db: Arc<DB>) -> Self {
        let sim_method = format!("{}_validateBuilderSubmissionV4", simulator_config.namespace);
        Self { http, simulator_config, db, sim_method }
    }

    /// Sends an RPC request for block validation.
    /// If `is_top_bid` = true, then the X-High-Priority header is set as "true"
    pub async fn send_rpc_request(
        &self,
        request: BlockSimRequest,
        is_top_bid: bool,
    ) -> Result<Response, reqwest::Error> {
        let mut headers = HeaderMap::new();
        if is_top_bid {
            headers.insert("X-High-Priority", HeaderValue::from_static("true"));
        };

        let rpc_payload = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": &self.sim_method,
            "params": [request]
        });

        debug!(
            request.message.slot,
            block_hash = %request.execution_payload.block_hash(),
            size = rpc_payload.to_string().len(),
            "Sending RPC request",
        );

        let res = self
            .http
            .post(&self.simulator_config.url)
            .headers(headers)
            .json(&rpc_payload)
            .send()
            .await;

        debug!(
            request.message.slot,
            block_hash = %request.execution_payload.block_hash(),
            size = rpc_payload.to_string().len(),
            "Sent RPC request",
        );

        res
    }

    /// Processes the response from the RPC call.
    pub async fn process_rpc_response(response: Response) -> Result<(), BlockSimError> {
        if response.status() != StatusCode::OK {
            return Err(BlockSimError::RpcError(response.status().to_string()));
        }

        match response.json::<BlockSimRpcResponse>().await {
            Ok(rpc_response) => {
                if let Some(error) = rpc_response.error {
                    return Err(BlockSimError::BlockValidationFailed(error.message));
                }
                Ok(())
            }
            Err(err) => Err(BlockSimError::RpcError(err.to_string())),
        }
    }

    pub async fn process_request(
        &self,
        request: BlockSimRequest,
        _builder_info: &BuilderInfo,
        is_top_bid: bool,
    ) -> Result<(), BlockSimError> {
        let timer = SimulatorMetrics::timer(&self.simulator_config.url);

        let block_hash = request.execution_payload.block_hash().0;
        debug!(
            %block_hash,
            builder_pub_key = %request.message.builder_pubkey,
            "RpcSimulator::process_request",
        );

        match self.send_rpc_request(request, is_top_bid).await {
            Ok(response) => {
                timer.stop_and_record();
                let result = Self::process_rpc_response(response).await;
                SimulatorMetrics::sim_status(result.is_ok());

                // Send sim result to db processor task
                let block_sim_result = result.clone();

                let db_clone = self.db.clone();
                task::spawn(file!(), line!(), {
                    async move {
                        if let Err(err) =
                            db_clone.save_simulation_result(block_hash, block_sim_result).await
                        {
                            error!(%err, "failed to store simulation result")
                        }
                    }
                    .in_current_span()
                });

                result
            }
            Err(err) => {
                timer.stop_and_discard();
                error!(?err, "Error sending RPC request");
                SimulatorMetrics::sim_status(false);
                Err(BlockSimError::RpcError(err.to_string()))
            }
        }
    }

    pub async fn is_synced(&self) -> Result<bool, BlockSimError> {
        // The JSON RPC payload for checking if Geth is syncing
        let payload = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "eth_syncing",
            "params": []
        });

        debug!(endpoint = %self.simulator_config.url, "sending eth_syncing");

        let response = match self.http.post(&self.simulator_config.url).json(&payload).send().await
        {
            Ok(response) => response,
            Err(err) => {
                error!(%err, "error sending eth_syncing request");
                return Err(BlockSimError::RpcError(err.to_string()));
            }
        };

        let json_response: Value = match response.json().await {
            Ok(json_response) => json_response,
            Err(err) => {
                error!(%err, "error parsing eth_syncing response");
                return Err(BlockSimError::RpcError(err.to_string()));
            }
        };

        // parse {"jsonrpc":"2.0","id":0,"result":false}
        let sync_result = &json_response["result"];
        match sync_result {
            // Fully synced
            Value::Bool(false) => Ok(true),
            // Still syncing, or unexpected format
            _ => Ok(false),
        }
    }

    pub async fn send_merge_request(
        &self,
        request: BlockMergeRequest,
    ) -> Result<Response, reqwest::Error> {
        let rpc_payload = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "relay_mergeBlockV1",
            "params": [request]
        });

        debug!(
            block_hash = %request.execution_payload.block_hash(),
            size = rpc_payload.to_string().len(),
            "Sending RPC merge request",
        );

        let res = self.http.post(&self.simulator_config.url).json(&rpc_payload).send().await;

        debug!(
            block_hash = %request.execution_payload.block_hash(),
            size = rpc_payload.to_string().len(),
            "Sent RPC merge request",
        );

        res
    }

    pub async fn process_merge_request(
        &self,
        request: BlockMergeRequest,
    ) -> Result<BlockMergeResponse, BlockSimError> {
        let timer = SimulatorMetrics::block_merge_timer(&self.simulator_config.url);

        let block_hash = request.execution_payload.block_hash().0;
        debug!(
            %block_hash,
            "RpcSimulator::process_merge_request",
        );

        match self.send_merge_request(request).await {
            Ok(response) => {
                timer.stop_and_record();
                let result = Self::process_merge_rpc_response(response).await;
                SimulatorMetrics::block_merge_status(result.is_ok());
                result
            }
            Err(err) => {
                timer.stop_and_discard();
                error!(?err, "Error sending RPC request");
                SimulatorMetrics::block_merge_status(false);
                Err(BlockSimError::RpcError(err.to_string()))
            }
        }
    }

    /// Processes the response from the RPC call.
    pub async fn process_merge_rpc_response(
        response: Response,
    ) -> Result<BlockMergeResponse, BlockSimError> {
        if response.status() != StatusCode::OK {
            return Err(BlockSimError::RpcError(response.status().to_string()));
        }

        match response.json::<RpcResult<BlockMergeResponse>>().await {
            Ok(rpc_response) => match rpc_response {
                RpcResult::Err { error } => {
                    Err(BlockSimError::BlockValidationFailed(error.message))
                }
                RpcResult::Ok(response) => Ok(response),
            },
            Err(err) => Err(BlockSimError::RpcError(err.to_string())),
        }
    }
}
