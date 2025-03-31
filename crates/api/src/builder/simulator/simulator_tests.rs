// ++++ IMPORTS ++++
use std::sync::Arc;

use alloy_primitives::hex;
use ethereum_consensus::{
    electra::ExecutionRequests, primitives::BlsSignature, ssz::prelude::*,
    types::mainnet::ExecutionPayload,
};
use helix_common::{
    bid_submission::{
        BidTrace, SignedBidSubmission, SignedBidSubmissionCapella, SignedBidSubmissionElectra,
    },
    deneb::BlobsBundle,
    electra::SignedBeaconBlock,
    simulator::BlockSimError,
    BuilderInfo, ValidatorPreferences,
};
use reqwest::Client;
use serde_json::json;

use crate::builder::{
    rpc_simulator::{BlockSimRpcResponse, JsonRpcError, RpcSimulator},
    traits::BlockSimulator,
    BlockSimRequest, DbInfo,
};

// ++++ HELPERS ++++
fn get_simulator(endpoint: &str) -> RpcSimulator {
    let http = Client::new();
    RpcSimulator::new(http, endpoint.to_string())
}

fn get_byte_vector_32_for_hex(hex: &str) -> ByteVector<32> {
    let bytes = hex::decode(&hex[2..]).unwrap();
    ByteVector::try_from(bytes.as_ref()).unwrap()
}

#[derive(Default, Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct BlockResponse {
    version: String,
    execution_optimistic: bool,
    finalized: bool,
    data: SignedBeaconBlock,
}

// -> BlockSimRequest later
async fn get_block(slot_number: u64) -> BlockSimRequest {
    let client = Client::new();
    let beacon_node_url = "http://localhost:5052/eth/v2/beacon/blocks";

    let url = format!("{}/{}", beacon_node_url, slot_number);
    let response = client.get(&url).send().await.unwrap();
    let block_response: BlockResponse = response.json().await.unwrap();

    let electra_exec_payload =
        ExecutionPayload::Electra(block_response.data.message.body.execution_payload);

    let bid_trace = BidTrace { ..Default::default() };
    let signed_bid_submission = SignedBidSubmission::Electra(SignedBidSubmissionElectra {
        message: bid_trace,
        signature: block_response.data.signature,
        execution_payload: electra_exec_payload,
        blobs_bundle: BlobsBundle::default(),
        execution_requests: ExecutionRequests::default(),
    });

    BlockSimRequest::new(
        30000000,
        Arc::new(signed_bid_submission),
        ValidatorPreferences::default(),
        Some(
            helix_common::bellatrix::ByteVector::try_from(
                block_response.data.message.parent_root.as_ref(),
            )
            .unwrap(),
        ),
    )
}

fn get_sim_req() -> BlockSimRequest {
    let capella_exec_payload = ethereum_consensus::capella::ExecutionPayload {
        block_hash: get_byte_vector_32_for_hex(
            "0x9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5",
        ),
        ..Default::default()
    };
    let execution_payload = ExecutionPayload::Capella(capella_exec_payload);
    let bid_trace = BidTrace {
        block_hash: get_byte_vector_32_for_hex(
            "0x9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5",
        ),
        ..Default::default()
    };
    let signed_bid_submission = SignedBidSubmission::Capella(SignedBidSubmissionCapella {
        message: bid_trace,
        signature: BlsSignature::default(),
        execution_payload,
    });

    BlockSimRequest::new(0, Arc::new(signed_bid_submission), ValidatorPreferences::default(), None)
}

// ++++ TESTS ++++
#[tokio::test]
async fn test_process_request_ok() {
    let mut server = mockito::Server::new();
    let mock = server
        .mock("POST", "/")
        .with_status(200)
        .with_body(r#"{"jsonrpc":"2.0","id":"1","result":true}"#)
        .create();

    let (sim_res_sender, mut sim_res_receiver) = tokio::sync::mpsc::channel(100);
    let simulator = get_simulator(&server.url());
    let builder_info = BuilderInfo::default();
    let result =
        simulator.process_request(get_sim_req(), &builder_info, true, sim_res_sender).await;

    mock.assert();
    assert!(result.is_ok());
    let received_sim_res = sim_res_receiver.recv().await.unwrap();
    match received_sim_res {
        DbInfo::SimulationResult { block_hash, block_sim_result } => {
            assert_eq!(
                block_hash,
                get_byte_vector_32_for_hex(
                    "0x9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5"
                )
            );
            assert!(block_sim_result.is_ok());
        }
        _ => panic!("Expected DbInfo::SimulationResult"),
    }
}

#[tokio::test]
async fn test_process_request_error() {
    let mut server = mockito::Server::new();
    let mock = server
        .mock("POST", "/")
        .with_status(400)
        .with_body(r#"{"jsonrpc":"2.0","id":"1","result":false}"#)
        .create();

    let (sim_res_sender, _sim_res_receiver) = tokio::sync::mpsc::channel(100);
    let simulator = get_simulator(&server.url());
    let builder_info = BuilderInfo::default();
    let result =
        simulator.process_request(get_sim_req(), &builder_info, true, sim_res_sender).await;

    mock.assert();
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), BlockSimError::RpcError(_)));
}

#[tokio::test]
#[ignore]
async fn test_process_request_from_beacon() -> Result<(), BlockSimError> {
    let block = get_block(3875710).await;
    let simulator = get_simulator("http://localhost:8545");
    match simulator.send_rpc_request(block, true).await {
        Ok(x) => match x.json::<BlockSimRpcResponse>().await {
            Ok(rpc_response) => {
                if let Some(error) = rpc_response.error {
                    return Err(BlockSimError::BlockValidationFailed(error.message));
                }
                Ok(())
            }
            Err(err) => Err(BlockSimError::RpcError(err.to_string())),
        },
        Err(e) => panic!("Error: {:?}", e),
    }
}

#[tokio::test]
async fn test_quickquci() {
    let x = "helllooooo";

    let formatted = format!("{x:?}");
    let formatted_2 = x.to_string();

    println!("{formatted}");
    println!("{formatted_2}");
}

#[tokio::test]
async fn test_process_request_validation_failed() {
    let rpc_response = BlockSimRpcResponse {
        error: Some(JsonRpcError { message: "validation failed".to_string() }),
    };
    let rpc_response_json = json!(rpc_response).to_string();
    let mut server = mockito::Server::new();
    let mock = server.mock("POST", "/").with_status(200).with_body(rpc_response_json).create();

    let (sim_res_sender, _sim_res_receiver) = tokio::sync::mpsc::channel(100);
    let simulator = get_simulator(&server.url());
    let builder_info = BuilderInfo::default();
    let result =
        simulator.process_request(get_sim_req(), &builder_info, true, sim_res_sender).await;

    mock.assert();
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), BlockSimError::BlockValidationFailed(_)));
}
