use std::sync::Arc;

use helix_common::{metrics::SimulatorMetrics, simulator::BlockSimError, task, BuilderInfo};
use helix_database::DatabaseService;
use reqwest::{
    header::{HeaderMap, HeaderValue, CONTENT_TYPE},
    Client, Response, StatusCode,
};
use serde_json::{json, Value};
use tracing::{debug, error, info, Instrument};

use crate::builder::{BlockSimRequest, DbInfo};

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
pub struct RpcSimulator<DB: DatabaseService + 'static> {
    http: Client,
    pub endpoint: String,
    db: Arc<DB>,
}

impl<DB: DatabaseService + 'static> RpcSimulator<DB> {
    pub fn new(http: Client, endpoint: String, db: Arc<DB>) -> Self {
        Self { http, endpoint, db }
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

    pub async fn process_request(
        &self,
        request: BlockSimRequest,
        _builder_info: &BuilderInfo,
        is_top_bid: bool,
    ) -> Result<bool, BlockSimError> {
        let timer = SimulatorMetrics::timer(&self.endpoint);

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
                let db_clone = self.db.clone();
                task::spawn(file!(), line!(), {
                    async move {
                        process_db_additions(db_clone, db_info).await;
                    }
                    .in_current_span()
                });

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

    pub async fn is_synced(&self) -> Result<bool, BlockSimError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        // The JSON RPC payload for checking if Geth is syncing
        let payload = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "eth_syncing",
            "params": []
        });

        debug!(endpoint = %self.endpoint, "sending eth_syncing");

        let response =
            match self.http.post(&self.endpoint).headers(headers).json(&payload).send().await {
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
}

/// Should be called as a new async task.
/// Stores updates to the db out of the critical path.
async fn process_db_additions<DB: DatabaseService + 'static>(db: Arc<DB>, db_info: DbInfo) {
    match db_info {
        DbInfo::NewSubmission(submission, trace, version) => {
            if let Err(err) = db.store_block_submission(submission, trace, version as i16).await {
                error!(%err, "failed to store block submission")
            }
        }
        DbInfo::NewHeaderSubmission(header_submission, trace) => {
            if let Err(err) = db.store_header_submission(header_submission, trace, None).await {
                error!(%err, "failed to store header submission")
            }
        }
        DbInfo::GossipedHeader { block_hash, trace } => {
            if let Err(err) = db.save_gossiped_header_trace(block_hash, trace).await {
                error!(%err, "failed to store gossiped header trace")
            }
        }
        DbInfo::GossipedPayload { block_hash, trace } => {
            if let Err(err) = db.save_gossiped_payload_trace(block_hash, trace).await {
                error!(%err, "failed to store gossiped payload trace")
            }
        }
        DbInfo::SimulationResult { block_hash, block_sim_result } => {
            if let Err(err) = db.save_simulation_result(block_hash, block_sim_result).await {
                error!(%err, "failed to store simulation result")
            }
        }
    }
}
