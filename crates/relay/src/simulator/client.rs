use helix_common::{
    SimulatorConfig,
    simulator::{BlockSimError, SszValidationResponse, TxDetail},
};
use helix_types::ForkName;
use serde_json::Value;
use ssz::Decode;
use tracing::error;

pub(super) const SYNC_REQUEST: &[u8] =
    br#"{"jsonrpc":"2.0","id":"1","method":"eth_syncing","params":[]}"#;

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
pub(super) struct JsonRpcRequest<'a, T> {
    jsonrpc: &'static str,
    id: &'static str,
    method: &'a str,
    params: [&'a T; 1],
}

impl<'a, T> JsonRpcRequest<'a, T> {
    pub(super) fn new(method: &'a str, params: &'a T) -> Self {
        Self { jsonrpc: "2.0", id: "1", method, params: [params] }
    }
}

/// Which validation route a simulator serves for each fork. The requests
/// themselves are sent by `SimulatorTile`.
#[derive(Clone)]
pub struct SimulatorClient {
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
    pub fn new(config: SimulatorConfig) -> Self {
        let sim_method_v4 = format!("{}_validateBuilderSubmissionV4", config.namespace);
        let sim_method_v5 = format!("{}_validateBuilderSubmissionV5", config.namespace);
        let sim_method_merged_v5 =
            format!("{}_validateMergedBuilderSubmissionV5", config.namespace);
        let ssz_url = config.ssz_url.clone();
        Self { config, sim_method_v4, sim_method_v5, sim_method_merged_v5, ssz_url }
    }

    pub fn endpoint(&self) -> &str {
        &self.config.url
    }

    /// Whether the SSZ validator handles `fork`. It has its own fork list,
    /// apart from [`Self::json_method`]: an SSZ validator is a different
    /// implementation from the JSON one.
    pub fn ssz_supports(&self, fork: ForkName) -> bool {
        self.ssz_url.is_some() && matches!(fork, ForkName::Fulu | ForkName::Gloas)
    }

    /// Returns `None` for a fork this client has no validation RPC method for yet, rather than
    /// silently mis-routing it to a method shaped for a different fork.
    pub fn json_method(&self, fork: ForkName) -> Option<&str> {
        match fork {
            ForkName::Fulu => Some(&self.sim_method_v5),
            ForkName::Bellatrix | ForkName::Capella | ForkName::Deneb | ForkName::Electra => {
                Some(&self.sim_method_v4)
            }
            ForkName::Base | ForkName::Altair | ForkName::Gloas | ForkName::Heze => None,
        }
    }

    /// Whether this simulator can validate a submission of `fork` at all. A
    /// simulator with an SSZ endpoint never falls back to JSON-RPC.
    pub fn can_validate(&self, fork: ForkName) -> bool {
        match self.ssz_url {
            Some(_) => self.ssz_supports(fork),
            None => self.json_method(fork).is_some(),
        }
    }

    pub(super) fn json_sim_outcome(status: u16, body: &[u8]) -> Result<(), BlockSimError> {
        if status >= 400 {
            error!(status, "failed rpc simulation");
            return Err(BlockSimError::RpcError);
        }
        let res: BlockSimRpcResponse = serde_json::from_slice(body).map_err(|err| {
            error!(%err, "failed rpc simulation");
            BlockSimError::RpcError
        })?;
        match res.error {
            Some(error) => Err(BlockSimError::BlockValidationFailed(error.message)),
            None => Ok(()),
        }
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

    pub(super) fn ssz_sim_outcome(
        status: u16,
        body: &[u8],
        endpoint: &str,
        request_len: usize,
    ) -> Result<Vec<TxDetail>, BlockSimError> {
        match status {
            200 => Ok(Self::validation_tx_details(body)),
            400 => Err(BlockSimError::BlockValidationFailed(
                String::from_utf8_lossy(body).into_owned(),
            )),
            413 => {
                error!(
                    endpoint,
                    body_len = request_len,
                    "ssz simulation request over the simulator body limit"
                );
                Err(BlockSimError::PayloadTooLarge)
            }
            424 => Err(BlockSimError::HydrationMiss),
            status => {
                let body = String::from_utf8_lossy(body);
                error!(status, %body, endpoint, "ssz simulation rejected by the simulator");
                Err(BlockSimError::RpcError)
            }
        }
    }

    /// `None` when the answer is unusable. `Some(false)` is the node still
    /// syncing, or an answer in an unexpected shape.
    pub(super) fn sync_outcome(status: u16, body: &[u8]) -> Option<bool> {
        if status >= 400 {
            return None;
        }
        let res: Value = serde_json::from_slice(body).ok()?;
        // {"jsonrpc":"2.0","id":0,"result":false}
        Some(matches!(res["result"], Value::Bool(false)))
    }
}

#[cfg(test)]
mod test {
    use alloy_primitives::B256;
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
        SimulatorClient::new(SimulatorConfig {
            url: "http://localhost:8545".into(),
            namespace: "relay".into(),
            max_concurrent_tasks: 1,
            ssz_url: None,
        })
    }

    #[test]
    fn routes_fulu_to_v5() {
        assert!(sim_client().json_method(ForkName::Fulu).unwrap().ends_with("V5"));
    }

    #[test]
    fn routes_electra_to_v4() {
        assert!(sim_client().json_method(ForkName::Electra).unwrap().ends_with("V4"));
    }

    #[test]
    fn gloas_has_no_validation_method_yet() {
        assert!(sim_client().json_method(ForkName::Gloas).is_none());
        assert!(!sim_client().can_validate(ForkName::Gloas));
    }

    fn ssz_client() -> super::SimulatorClient {
        super::SimulatorClient::new(SimulatorConfig {
            url: "http://localhost:8545".into(),
            namespace: "relay".into(),
            max_concurrent_tasks: 1,
            ssz_url: Some("http://localhost:8552".into()),
        })
    }

    #[test]
    fn ssz_route_takes_fulu() {
        assert!(ssz_client().can_validate(ForkName::Fulu));
    }

    #[test]
    fn ssz_route_takes_gloas() {
        assert!(ssz_client().can_validate(ForkName::Gloas));
    }

    #[test]
    fn ssz_route_refuses_an_unscheduled_fork() {
        assert!(
            !ssz_client().can_validate(ForkName::Heze),
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
}
