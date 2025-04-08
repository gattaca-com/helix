use async_trait::async_trait;
use helix_common::{
    metrics::{SimulatorMetrics, DB_QUEUE},
    simulator::BlockSimError,
    BuilderInfo,
};
use reqwest::{
    header::{HeaderMap, HeaderValue, CONTENT_TYPE},
    Client, Response, StatusCode,
};
use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, info};

use crate::builder::{traits::BlockSimulator, BlockSimRequest, DbInfo};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcError {
    pub message: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct BlockSimRpcResponse {
    pub error: Option<JsonRpcError>,
}

/// RpcSimulator is responsible for sending block requests to the RPC endpoint for validation.
/// It uses the `flashbots_validateBuilderSubmissionV2` method for the actual validation.
#[derive(Clone)]
pub struct RpcSimulator {
    http: Client,
    endpoint: String,
}

impl RpcSimulator {
    pub fn new(http: Client, endpoint: String) -> Self {
        Self { http, endpoint }
    }

    /// Sends an RPC request for block validation.
    /// If `is_top_bid` = true, then the X-High-Priority header is set as "true"
    pub async fn send_rpc_request(
        &self,
        request: BlockSimRequest,
        is_top_bid: bool,
    ) -> Result<Response, reqwest::Error> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if is_top_bid {
            headers.insert("X-High-Priority", HeaderValue::from_static("true"));
        };

        let rpc_payload = if request.parent_beacon_block_root.is_none() {
            json!({
                "jsonrpc": "2.0",
                "id": "1",
                "method": "flashbots_validateBuilderSubmissionV2",
                "params": [request]
            })
        } else if request.execution_requests.is_none() {
            json!({
                "jsonrpc": "2.0",
                "id": "1",
                "method": "flashbots_validateBuilderSubmissionV3",
                "params": [request]
            })
        } else {
            json!({
                "jsonrpc": "2.0",
                "id": "1",
                "method": "flashbots_validateBuilderSubmissionV4",
                "params": [request]
            })
        };

        info!(
            request.message.slot,
            block_hash = %request.execution_payload.block_hash(),
            size = rpc_payload.to_string().len(),
            "Sending RPC request",
        );

        let res = self.http.post(&self.endpoint).headers(headers).json(&rpc_payload).send().await;

        info!(
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
}

#[async_trait]
impl BlockSimulator for RpcSimulator {
    async fn process_request(
        &self,
        request: BlockSimRequest,
        _builder_info: &BuilderInfo,
        is_top_bid: bool,
        sim_result_saver_sender: Sender<DbInfo>,
    ) -> Result<bool, BlockSimError> {
        let timer = SimulatorMetrics::timer();

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
                let db_info =
                    DbInfo::SimulationResult { block_hash, block_sim_result: result.clone() };
                sim_result_saver_sender
                    .send(db_info)
                    .await
                    .map_err(|_| BlockSimError::SendError)?;
                DB_QUEUE.inc();

                result.map(|_| false)
            }
            Err(err) => {
                timer.stop_and_discard();
                error!(?err, "Error sending RPC request");
                SimulatorMetrics::sim_status(false);
                Err(BlockSimError::RpcError(err.to_string()))
            }
        }
    }

    async fn is_synced(&self) -> Result<bool, BlockSimError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        // The JSON RPC payload for checking if Geth is syncing
        let payload = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "eth_syncing",
            "params": []
        });

        info!("Sending eth_syncing request to check Geth sync status...");

        let response =
            match self.http.post(&self.endpoint).headers(headers).json(&payload).send().await {
                Ok(response) => response,
                Err(err) => {
                    error!("Error sending eth_syncing request: {:?}", err);
                    return Err(BlockSimError::RpcError(err.to_string()));
                }
            };

        let json_response: Value = match response.json().await {
            Ok(json_response) => json_response,
            Err(err) => {
                error!("Error parsing eth_syncing response: {:?}", err);
                return Err(BlockSimError::RpcError(err.to_string()));
            }
        };

        info!("Received response: {:?}", json_response);

        // According to the Ethereum JSON-RPC spec, if Geth is fully synced,
        // `eth_syncing` will return "result": false
        let sync_result = &json_response["result"];
        match sync_result {
            // Fully synced
            Value::Bool(false) => {
                info!("Geth is fully synced.");
                Ok(true)
            }
            // Still syncing, or unexpected format
            _ => {
                info!("Geth is not fully synced: {:?}", sync_result);
                Ok(false)
            }
        }
    }
}
