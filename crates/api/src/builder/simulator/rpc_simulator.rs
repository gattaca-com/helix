use async_trait::async_trait;
use helix_common::BuilderInfo;
use reqwest::{
    header::{HeaderMap, HeaderValue, CONTENT_TYPE},
    Client, Response, StatusCode,
};
use serde_json::json;
use tokio::sync::mpsc::Sender;
use tracing::{debug, error};

use helix_common::simulator::BlockSimError;
use uuid::Uuid;

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
    async fn send_rpc_request(
        &self,
        request: BlockSimRequest,
        is_top_bid: bool,
    ) -> Result<Response, reqwest::Error> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if is_top_bid {
            headers.insert("X-High-Priority", HeaderValue::from_static("true"));
        }

        let rpc_payload = if request.parent_beacon_block_root.is_none() {
            json!({
                "jsonrpc": "2.0",
                "id": "1",
                "method": "flashbots_validateBuilderSubmissionV2",
                "params": [request]
            })
        } else {
            json!({
                "jsonrpc": "2.0",
                "id": "1",
                "method": "flashbots_validateBuilderSubmissionV3",
                "params": [request]
            })
        };

        self.http.post(&self.endpoint).headers(headers).json(&rpc_payload).send().await
    }

    /// Processes the response from the RPC call.
    async fn process_rpc_response(response: Response) -> Result<(), BlockSimError> {
        if response.status() != StatusCode::OK {
            return Err(BlockSimError::RpcError(response.status().to_string()))
        }

        match response.json::<BlockSimRpcResponse>().await {
            Ok(rpc_response) => {
                if let Some(error) = rpc_response.error {
                    return Err(BlockSimError::BlockValidationFailed(error.message))
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
        request_id: Uuid,
    ) -> Result<bool, BlockSimError> {
        let block_hash = request.execution_payload.block_hash().clone();
        debug!(
            request_id = %request_id,
            block_hash = %block_hash,
            builder_pub_key = %request.message.builder_public_key,
            "RpcSimulator::process_request",
        );

        match self.send_rpc_request(request, is_top_bid).await {
            Ok(response) => {
                let result = Self::process_rpc_response(response).await;

                // Send sim result to db processor task
                let db_info =
                    DbInfo::SimulationResult { block_hash, block_sim_result: result.clone() };
                sim_result_saver_sender
                    .send(db_info)
                    .await
                    .map_err(|_| BlockSimError::SendError)?;

                result.map(|_| false)
            }
            Err(err) => {
                error!(request_id = %request_id, err = ?err, "Error sending RPC request");
                Err(BlockSimError::RpcError(err.to_string()))
            }
        }
    }
}
