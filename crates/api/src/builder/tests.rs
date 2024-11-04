// +++ IMPORTS +++
use crate::{
    builder::{
        api::{decode_header_submission, decode_payload, BuilderApi, MAX_PAYLOAD_LENGTH},
        mock_simulator::MockSimulator,
    },
    gossiper::mock_gossiper::MockGossiper,
    service::API_REQUEST_TIMEOUT,
    test_utils::builder_api_app,
};
use axum::http::{header, Method, Request, Uri};
use core::panic;
use ethereum_consensus::{
    builder::{SignedValidatorRegistration, ValidatorRegistration},
    configs::mainnet::CAPELLA_FORK_EPOCH,
    deneb::Withdrawal,
    phase0::mainnet::SLOTS_PER_EPOCH,
    primitives::{BlsPublicKey, BlsSignature},
    ssz::{self, prelude::*},
    types::mainnet::{ExecutionPayload, ExecutionPayloadHeader},
    Fork,
};
use futures::{stream::FuturesOrdered, Future, SinkExt, StreamExt};
use helix_beacon_client::types::PayloadAttributes;
use helix_common::{
    api::{
        builder_api::{
            BuilderGetValidatorsResponse, BuilderGetValidatorsResponseEntry, TopBidUpdate,
        },
        proposer_api::ValidatorRegistrationInfo,
    },
    bid_submission::{
        v2::header_submission::{
            SignedHeaderSubmission, SignedHeaderSubmissionCapella, SignedHeaderSubmissionDeneb,
        },
        BidSubmission, SignedBidSubmission,
    },
    HeaderSubmissionTrace, Route, SubmissionTrace, ValidatorPreferences,
};
use helix_database::MockDatabaseService;
use helix_datastore::MockAuctioneer;
use helix_housekeeper::{ChainUpdate, PayloadAttributesUpdate, SlotUpdate};
use helix_utils::{calculate_withdrawals_root, request_encoding::Encoding};
use rand::Rng;
use reqwest::{Client, Response};
use reth_primitives::hex;
use serde_json::json;
use serial_test::serial;
use std::{
    convert::Infallible, future::pending, io::Write, ops::Deref, pin::Pin, str::FromStr, sync::Arc,
    time::Duration,
};
use tokio::sync::{
    mpsc::{Receiver, Sender},
    oneshot,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, Message},
};
use tonic::transport::Body;

// +++ HELPER VARIABLES +++
const ADDRESS: &str = "0.0.0.0";
const PORT: u16 = 3000;
const HEAD_SLOT: u64 = 32; //ethereum_consensus::configs::mainnet::CAPELLA_FORK_EPOCH;
const SUBMISSION_SLOT: u64 = HEAD_SLOT + 1;
const SUBMISSION_TIMESTAMP: u64 = 1606824419;
const VALIDATOR_INDEX: usize = 1;

// +++ HELPER FUNCTIONS +++

#[derive(Debug, Clone)]
struct HttpServiceConfig {
    address: String,
    port: u16,
}

impl HttpServiceConfig {
    fn new(address: &str, port: u16) -> Self {
        HttpServiceConfig { address: address.to_string(), port }
    }

    fn base_url(&self) -> String {
        format!("http://{}:{}", self.address, self.port)
    }

    fn bind_address(&self) -> String {
        format!("{}:{}", self.address, self.port)
    }
}

async fn send_request(req_url: &str, encoding: Encoding, req_payload: Vec<u8>) -> Response {
    let client = Client::new();
    let request = client.post(req_url).header("accept", "*/*");
    let request = encoding.to_headers(request);

    request.body(req_payload).send().await.unwrap()
}

fn get_test_pub_key_bytes(random: bool) -> [u8; 48] {
    if random {
        let mut pubkey_array = [0u8; 48];
        rand::thread_rng().fill(&mut pubkey_array[..]);
        pubkey_array
    } else {
        let pubkey_hex = "0x84e975405f8691ad7118527ee9ee4ed2e4e8bae973f6e29aa9ca9ee4aea83605ae3536d22acc9aa1af0545064eacf82e";
        let pubkey_bytes = hex::decode(&pubkey_hex[2..]).unwrap();
        let mut pubkey_array = [0u8; 48];
        pubkey_array.copy_from_slice(&pubkey_bytes);
        pubkey_array
    }
}

fn get_byte_vector_20_for_hex(hex: &str) -> ByteVector<20> {
    let bytes = hex::decode(&hex[2..]).unwrap();
    ByteVector::try_from(bytes.as_ref()).unwrap()
}

fn get_byte_vector_32_for_hex(hex: &str) -> ByteVector<32> {
    let bytes = hex::decode(&hex[2..]).unwrap();
    ByteVector::try_from(bytes.as_ref()).unwrap()
}

fn hex_to_byte_arr_32(hex: &str) -> [u8; 32] {
    let bytes = hex::decode(&hex[2..]).unwrap();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    arr
}

fn get_valid_payload_register_validator(
    submission_slot: Option<u64>,
) -> BuilderGetValidatorsResponseEntry {
    BuilderGetValidatorsResponseEntry {
            slot: submission_slot.unwrap_or(SUBMISSION_SLOT),
            validator_index: VALIDATOR_INDEX,
            entry: ValidatorRegistrationInfo {
                registration: SignedValidatorRegistration {
                    message: ValidatorRegistration {
                        fee_recipient: get_byte_vector_20_for_hex("0x5cc0dde14e7256340cc820415a6022a7d1c93a35"),
                        gas_limit: 30000000,
                        timestamp: SUBMISSION_TIMESTAMP,
                        public_key: BlsPublicKey::try_from(&get_test_pub_key_bytes(false)[..]).unwrap(),
                    },
                    signature: BlsSignature::try_from(hex::decode(&"0xaf12df007a0c78abb5575067e5f8b089cfcc6227e4a91db7dd8cf517fe86fb944ead859f0781277d9b78c672e4a18c5d06368b603374673cf2007966cece9540f3a1b3f6f9e1bf421d779c4e8010368e6aac134649c7a009210780d401a778a5"[2..]).unwrap().as_slice()).unwrap(),
                },
                preferences: ValidatorPreferences::default(),
            }
        }
}

fn get_dummy_slot_update(head_slot: Option<u64>, submission_slot: Option<u64>) -> SlotUpdate {
    SlotUpdate {
        slot: head_slot.unwrap_or(HEAD_SLOT),
        next_duty: Some(get_valid_payload_register_validator(submission_slot)),
        new_duties: Some(vec![get_valid_payload_register_validator(submission_slot)]),
    }
}

fn get_dummy_payload_attributes() -> PayloadAttributes {
    PayloadAttributes {
        timestamp: SUBMISSION_TIMESTAMP,
        prev_randao: get_byte_vector_32_for_hex(
            "0x9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e4",
        ),
        suggested_fee_recipient: "0x5cc0dde14e7256340cc820415a6022a7d1c93a35".to_string(),
        withdrawals: vec![],
        parent_beacon_block_root: None,
    }
}

fn get_dummy_payload_attributes_update(submission_slot: Option<u64>) -> PayloadAttributesUpdate {
    PayloadAttributesUpdate {
        slot: submission_slot.unwrap_or(SUBMISSION_SLOT),
        parent_hash: get_byte_vector_32_for_hex(
            "0xbd3291854dc822b7ec585925cda0e18f06af28fa2886e15f52d52dd4b6f94ed6",
        ),
        withdrawals_root: None,
        payload_attributes: get_dummy_payload_attributes(),
    }
}

async fn send_dummy_slot_update(
    slot_update_sender: Sender<ChainUpdate>,
    head_slot: Option<u64>,
    submission_slot: Option<u64>,
) {
    let chain_update = ChainUpdate::SlotUpdate(get_dummy_slot_update(head_slot, submission_slot));
    slot_update_sender.send(chain_update).await.unwrap();

    // sleep for a bit to allow the api to process the slot update
    tokio::time::sleep(Duration::from_millis(100)).await;
}

async fn send_dummy_payload_attributes_update(
    slot_update_sender: Sender<ChainUpdate>,
    submission_slot: Option<u64>,
) {
    let chain_update =
        ChainUpdate::PayloadAttributesUpdate(get_dummy_payload_attributes_update(submission_slot));
    slot_update_sender.send(chain_update).await.unwrap();

    // sleep for a bit to allow the api to process the slot update
    tokio::time::sleep(Duration::from_millis(100)).await;
}

fn load_bid_submission() -> SignedBidSubmission {
    let mut current_dir = std::env::current_dir().expect("Failed to get current directory");
    if !current_dir.ends_with("api") {
        current_dir.push("crates/api/");
    }
    current_dir.push("test_data/submitBlockPayloadCapella_Goerli.json.gz");
    let req_payload_bytes =
        load_gzipped_bytes(current_dir.to_str().expect("Failed to convert path to string"));
    let mut signed_bid_submission: SignedBidSubmission =
        serde_json::from_slice(&req_payload_bytes).unwrap();

    // set the slot and timestamp
    signed_bid_submission.message_mut().slot = SUBMISSION_SLOT;
    match &mut signed_bid_submission.execution_payload_mut() {
        ExecutionPayload::Capella(ref mut payload) => {
            payload.timestamp = SUBMISSION_TIMESTAMP;
        }
        ExecutionPayload::Bellatrix(ref mut payload) => {
            payload.timestamp = SUBMISSION_TIMESTAMP;
        }
        _ => panic!("unexpected execution payload type"),
    }

    signed_bid_submission
}

fn load_bid_submission_from_file(
    filename: &str,
    submission_slot: Option<u64>,
    submission_timestamp: Option<u64>,
) -> SignedBidSubmission {
    let mut current_dir = std::env::current_dir().expect("Failed to get current directory");
    if !current_dir.ends_with("api") {
        current_dir.push("crates/api/");
    }
    current_dir.push("test_data/");
    current_dir.push(filename);
    let req_payload_bytes =
        load_bytes(current_dir.to_str().expect("Failed to convert path to string"));
    let mut signed_bid_submission: SignedBidSubmission =
        serde_json::from_slice(&req_payload_bytes).unwrap();

    // set the slot and timestamp
    signed_bid_submission.message_mut().slot = submission_slot.unwrap_or(SUBMISSION_SLOT);
    match &mut signed_bid_submission.execution_payload_mut() {
        ExecutionPayload::Capella(ref mut payload) => {
            payload.timestamp = submission_timestamp.unwrap_or(SUBMISSION_TIMESTAMP);
        }
        ExecutionPayload::Bellatrix(ref mut payload) => {
            payload.timestamp = submission_timestamp.unwrap_or(SUBMISSION_TIMESTAMP);
        }
        _ => panic!("unexpected execution payload type"),
    }

    signed_bid_submission
}

fn load_gzipped_bytes(filename: &str) -> Vec<u8> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    let mut file = std::fs::File::open(filename).unwrap();
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).unwrap();

    let mut decoder = GzDecoder::new(&buffer[..]);
    let mut decoded_buffer = Vec::new();
    decoder.read_to_end(&mut decoded_buffer).unwrap();

    decoded_buffer
}

fn load_bytes(filename: &str) -> Vec<u8> {
    use std::io::Read;

    let mut file = std::fs::File::open(filename).unwrap();
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).unwrap();

    buffer
}

async fn start_api_server() -> (
    oneshot::Sender<()>,
    HttpServiceConfig,
    Arc<BuilderApi<MockAuctioneer, MockDatabaseService, MockSimulator, MockGossiper>>,
    Receiver<Sender<ChainUpdate>>,
) {
    let (tx, rx) = oneshot::channel();
    let http_config = HttpServiceConfig::new(ADDRESS, PORT);
    let bind_address = http_config.bind_address();

    let (router, api, slot_update_receiver) = builder_api_app();

    // Run the app in a background task
    tokio::spawn(async move {
        // run it with hyper on localhost:3000
        let listener = tokio::net::TcpListener::bind(bind_address).await.unwrap();
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                rx.await.ok();
            })
            .await
            .unwrap();
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    (tx, http_config, api, slot_update_receiver)
}

fn _get_req_body_submit_block_json() -> serde_json::Value {
    json!({
        "message": {
            "slot": "1",
            "parent_hash": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
            "block_hash": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
            "builder_pubkey": "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a",
            "proposer_fee_recipient": "0xabcf8e0d4e9587369b2301d0790347320302cc09",
            "gas_limit": "1",
            "gas_used": "1",
            "value": "1"
        },
        "execution_payload": {
            "parent_hash": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
            "fee_recipient": "0xabcf8e0d4e9587369b2301d0790347320302cc09",
            "state_root": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
            "receipts_root": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
            "logs_bloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
            "prev_randao": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
            "block_number": "1",
            "gas_limit": "1",
            "gas_used": "1",
            "timestamp": "1",
            "extra_data": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
            "base_fee_per_gas": "1",
            "block_hash": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
            "transactions": [
                "0x02f878831469668303f51d843b9ac9f9843b9aca0082520894c93269b73096998db66be0441e836d873535cb9c8894a19041886f000080c001a031cc29234036afbf9a1fb9476b463367cb1f957ac0b919b69bbc798436e604aaa018c4e9c3914eb27aadd0b91e10b18655739fcf8c1fc398763a9f1beecb8ddc86"
            ],
            "withdrawals": [
                {
                    "index": "1",
                    "validator_index": "1",
                    "address": "0xabcf8e0d4e9587369b2301d0790347320302cc09",
                    "amount": "32000000000"
                }
            ]
        },
        "signature": "0x1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505cc411d61252fb6cb3fa0017b679f8bb2305b26a285fa2737f175668d0dff91cc1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505"
    })
}

pub fn generate_request(
    cancellations_enabled: bool,
    gzip_encoding: bool,
    ssz_content_type: bool,
    payload: &[u8],
) -> Request<axum::body::Body> {
    // Construct the URI with cancellations query parameter
    let uri_str = if cancellations_enabled {
        "http://example.com?cancellations=1"
    } else {
        "http://example.com"
    };
    let uri = Uri::from_str(uri_str).unwrap();

    // Construct the request method and body
    let method = Method::POST;
    let body = axum::body::Body::from(payload.to_vec());

    // Create the request builder
    let mut request_builder = Request::builder().method(method).uri(uri);

    // Add headers based on flags
    if gzip_encoding {
        request_builder = request_builder.header(header::CONTENT_ENCODING, "gzip");
    }
    if ssz_content_type {
        request_builder = request_builder.header(header::CONTENT_TYPE, "application/octet-stream");
    } else {
        request_builder = request_builder.header(header::CONTENT_TYPE, "application/json");
    }

    // Build the request
    request_builder.body(body).unwrap()
}

// +++ TESTS +++

#[tokio::test]
async fn test_header_submission_decoding_json_capella() {
    let mut current_dir = std::env::current_dir().expect("Failed to get current directory");
    if !current_dir.ends_with("api") {
        current_dir.push("crates/api/");
    }
    current_dir.push("test_data/submitBlockPayloadHeaderCapella.json");
    let req_payload_bytes =
        load_bytes(current_dir.to_str().expect("Failed to convert path to string"));

    let mut header_submission_trace = HeaderSubmissionTrace::default();
    let uuid = uuid::Uuid::new_v4();
    let request = generate_request(false, false, false, &req_payload_bytes);
    let decoded_submission =
        decode_header_submission(request, &mut header_submission_trace, &uuid).await.unwrap();

    assert_eq!(decoded_submission.0.slot(), 5552306);
    assert!(matches!(
        decoded_submission.0.execution_payload_header(),
        ExecutionPayloadHeader::Capella(_)
    ));
    assert!(decoded_submission.0.commitments().is_none());
}

#[tokio::test]
async fn test_header_submission_decoding_json_deneb() {
    let mut current_dir = std::env::current_dir().expect("Failed to get current directory");
    if !current_dir.ends_with("api") {
        current_dir.push("crates/api/");
    }
    current_dir.push("test_data/submitBlockPayloadHeaderDeneb.json");
    let req_payload_bytes =
        load_bytes(current_dir.to_str().expect("Failed to convert path to string"));

    let decoded_submission: SignedHeaderSubmissionDeneb =
        serde_json::from_slice(&req_payload_bytes).unwrap();

    assert_eq!(decoded_submission.message.bid_trace().slot, 5552306);
}

#[tokio::test]
async fn test_header_submission_decoding_ssz_capella() {
    let mut current_dir = std::env::current_dir().expect("Failed to get current directory");
    if !current_dir.ends_with("api") {
        current_dir.push("crates/api/");
    }
    current_dir.push("test_data/header_submission_capella_ssz_bytes");
    let req_payload_bytes =
        load_bytes(current_dir.to_str().expect("Failed to convert path to string"));

    let mut header_submission_trace = HeaderSubmissionTrace::default();
    let uuid = uuid::Uuid::new_v4();
    let request = generate_request(false, false, true, &req_payload_bytes);
    let decoded_submission =
        decode_header_submission(request, &mut header_submission_trace, &uuid).await.unwrap();

    assert!(matches!(decoded_submission.0, SignedHeaderSubmission::Capella(_)));
    assert!(decoded_submission.0.commitments().is_none());

    let header: SignedHeaderSubmissionCapella =
        ssz::prelude::deserialize(&req_payload_bytes).unwrap();
    println!("{:?}", header);
}

#[tokio::test]
async fn test_header_submission_decoding_ssz_deneb() {
    let mut current_dir = std::env::current_dir().expect("Failed to get current directory");
    if !current_dir.ends_with("api") {
        current_dir.push("crates/api/");
    }
    current_dir.push("test_data/header_submission_deneb_ssz_bytes");
    let req_payload_bytes =
        load_bytes(current_dir.to_str().expect("Failed to convert path to string"));

    let mut header_submission_trace = HeaderSubmissionTrace::default();
    let uuid = uuid::Uuid::new_v4();
    let request = generate_request(false, false, true, &req_payload_bytes);
    let decoded_submission =
        decode_header_submission(request, &mut header_submission_trace, &uuid).await.unwrap();

    assert!(matches!(decoded_submission.0, SignedHeaderSubmission::Deneb(_)));
    assert!(decoded_submission.0.commitments().is_some());

    let header: SignedHeaderSubmissionDeneb =
        ssz::prelude::deserialize(&req_payload_bytes).unwrap();
    println!("{:?}", header);
}

#[tokio::test]
async fn test_signed_bid_submission_decoding_capella() {
    let mut current_dir = std::env::current_dir().expect("Failed to get current directory");
    if !current_dir.ends_with("api") {
        current_dir.push("crates/api/");
    }
    current_dir.push("test_data/submitBlockPayloadCapella_Goerli.json");
    let req_payload_bytes =
        load_bytes(current_dir.to_str().expect("Failed to convert path to string"));

    let mut submission_trace = SubmissionTrace::default();
    let uuid = uuid::Uuid::new_v4();
    let request = generate_request(false, false, false, &req_payload_bytes);
    let decoded_submission = decode_payload(request, &mut submission_trace, &uuid).await.unwrap();

    assert_eq!(decoded_submission.0.message().slot, 5552306);
    assert!(matches!(decoded_submission.0.execution_payload(), ExecutionPayload::Capella(_)));
    assert!(matches!(decoded_submission.0.execution_payload().version(), Fork::Capella));
    assert!(decoded_submission.0.blobs_bundle().is_none());
}

#[tokio::test]
async fn test_signed_bid_submission_decoding_capella_gzip() {
    let mut current_dir = std::env::current_dir().expect("Failed to get current directory");
    if !current_dir.ends_with("api") {
        current_dir.push("crates/api/");
    }
    current_dir.push("test_data/submitBlockPayloadCapella_Goerli.json.gz");
    let req_payload_bytes =
        load_bytes(current_dir.to_str().expect("Failed to convert path to string"));

    let mut submission_trace = SubmissionTrace::default();
    let uuid = uuid::Uuid::new_v4();
    let request = generate_request(false, true, false, &req_payload_bytes);
    let decoded_submission = decode_payload(request, &mut submission_trace, &uuid).await.unwrap();

    assert_eq!(decoded_submission.0.message().slot, 5552306);
    assert!(matches!(decoded_submission.0.execution_payload(), ExecutionPayload::Capella(_)));
    assert!(matches!(decoded_submission.0.execution_payload().version(), Fork::Capella));
    assert!(decoded_submission.0.blobs_bundle().is_none());
}

#[tokio::test]
async fn test_signed_bid_submission_decoding_deneb() {
    let mut current_dir = std::env::current_dir().expect("Failed to get current directory");
    if !current_dir.ends_with("api") {
        current_dir.push("crates/api/");
    }
    current_dir.push("test_data/submitBlockPayloadDeneb.json");
    let req_payload_bytes =
        load_bytes(current_dir.to_str().expect("Failed to convert path to string"));

    let mut submission_trace = SubmissionTrace::default();
    let uuid = uuid::Uuid::new_v4();
    let request = generate_request(false, false, false, &req_payload_bytes);
    let (decoded_submission, _) =
        decode_payload(request, &mut submission_trace, &uuid).await.unwrap();

    assert_eq!(decoded_submission.message().slot, 5552306);
    assert!(matches!(decoded_submission.execution_payload(), ExecutionPayload::Deneb(_)));
    assert!(matches!(decoded_submission.execution_payload().version(), Fork::Deneb));
    let deneb_payload = decoded_submission.execution_payload().deneb().unwrap();
    assert_eq!(deneb_payload.blob_gas_used, 100);
    assert_eq!(deneb_payload.excess_blob_gas, 50);
    assert!(decoded_submission.blobs_bundle().is_some());
}

#[tokio::test]
#[serial]
async fn test_get_validators_internal_server_error() {
    let (tx, http_config, _api, _slot_update_receiver) = start_api_server().await;

    // GET validators
    let req_url = format!("{}{}", http_config.base_url(), Route::GetValidators.path());
    let resp = reqwest::Client::new().get(req_url.as_str()).send().await.unwrap();

    // Check the response
    assert_eq!(resp.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR); // proposer duty bytes is None

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_get_validators_ok() {
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send a slot update
    // wait for the slot update to be received
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender, None, None).await;

    // GET validators
    let req_url = format!("{}{}", http_config.base_url(), Route::GetValidators.path());
    let resp = reqwest::Client::new().get(req_url.as_str()).send().await.unwrap();

    // Check the response
    assert_eq!(resp.status(), reqwest::StatusCode::OK); // proposer duty bytes is set

    // assert the body is the bytes of the new duties
    let body = resp.bytes().await.unwrap();

    let expected_response = get_dummy_slot_update(None, None).new_duties.unwrap();
    let expected_response: Vec<BuilderGetValidatorsResponse> =
        expected_response.into_iter().map(|item| item.into()).collect();

    let expected_json_bytes = serde_json::to_string(&expected_response).unwrap();

    assert_eq!(body, expected_json_bytes);

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_submit_block_invalid_signature() {
    // Start the server
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send slot & payload attributes updates
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let cancellations_enabled = false;
    let req_url = format!(
        "{}{}{}",
        http_config.base_url(),
        Route::SubmitBlock.path(),
        if cancellations_enabled { "?cancellations=1" } else { "" }
    );

    let mut signed_bid_submission: SignedBidSubmission = load_bid_submission();
    signed_bid_submission.message_mut().proposer_public_key =
        get_valid_payload_register_validator(None).entry.registration.message.public_key;

    // Send JSON encoded request
    let resp =
        send_request(&req_url, Encoding::Json, serde_json::to_vec(&signed_bid_submission).unwrap())
            .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Signature verification failed");

    // Send SSZ encoded request
    let resp = send_request(
        &req_url,
        Encoding::Ssz,
        ssz::prelude::serialize(&signed_bid_submission).unwrap(),
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Signature verification failed");

    // Send JSON+GZIP encoded request
    let mut req_payload_bytes = serde_json::to_vec(&signed_bid_submission).unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&req_payload_bytes).unwrap();
    req_payload_bytes = encoder.finish().unwrap();
    let resp = send_request(&req_url, Encoding::JsonGzip, req_payload_bytes.clone()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Signature verification failed");

    // Send SSZ+GZIP encoded request
    let req_payload_bytes = ssz::prelude::serialize(&signed_bid_submission).unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&req_payload_bytes).unwrap();
    let req_payload_bytes = encoder.finish().unwrap();
    let resp = send_request(&req_url, Encoding::SszGzip, req_payload_bytes).await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Signature verification failed");

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_submit_block_fee_recipient_mismatch() {
    // Start the server
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send slot & payload attributes updates
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let cancellations_enabled = false;
    let req_url = format!(
        "{}{}{}",
        http_config.base_url(),
        Route::SubmitBlock.path(),
        if cancellations_enabled { "?cancellations=1" } else { "" }
    );

    let mut signed_bid_submission: SignedBidSubmission = load_bid_submission();

    // Set incorrect fee recipient
    signed_bid_submission.message_mut().proposer_fee_recipient =
        get_byte_vector_20_for_hex("0x1230dde14e7256340cc820415a6022a7d1c93a35");

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .json(&signed_bid_submission)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Fee recipient mismatch. got: 0x1230dde14e7256340cc820415a6022a7d1c93a35, expected: 0x5cc0dde14e7256340cc820415a6022a7d1c93a35");

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_submit_block_submission_for_past_slot() {
    // Start the server
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send slot & payload attributes updates
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), Some(100), None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let signed_bid_submission: SignedBidSubmission = load_bid_submission();

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .json(&signed_bid_submission)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.text().await.unwrap(),
        "Submission for past slot. current slot: 100, submission slot: 33"
    );

    // Shut down the server
    let _ = tx.send(());
}

// TODO: fix this test. This test no longer works as we now check the signature
// before we sanity check
#[tokio::test]
#[ignore]
#[serial]
async fn test_submit_block_unknown_proposer_duty() {
    // Start the server
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send slot & payload attributes updates
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let mut signed_bid_submission: SignedBidSubmission = load_bid_submission();
    signed_bid_submission.message_mut().slot = 1;

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .json(&signed_bid_submission)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Could not find proposer duty for slot");

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_submit_block_incorrect_timestamp() {
    // Start the server
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send slot & payload attributes updates
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let mut signed_bid_submission: SignedBidSubmission = load_bid_submission();
    match signed_bid_submission.execution_payload_mut() {
        ExecutionPayload::Capella(ref mut payload) => {
            payload.timestamp = 1;
        }
        ExecutionPayload::Bellatrix(ref mut payload) => {
            payload.timestamp = 1;
        }
        _ => panic!("unexpected execution payload type"),
    }

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .json(&signed_bid_submission)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Incorrect timestamp. got: 1, expected: 1606824419");

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_submit_block_slot_mismatch() {
    // Start the server
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send slot & payload attributes updates
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, Some(1)).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let signed_bid_submission: SignedBidSubmission = load_bid_submission();

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .json(&signed_bid_submission)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Slot mismatch. got: 33, expected: 1");

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_submit_prev_randao_mismatch() {
    // Start the server
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send slot & payload attributes updates
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let mut signed_bid_submission: SignedBidSubmission = load_bid_submission();
    match signed_bid_submission.execution_payload_mut() {
        ExecutionPayload::Capella(ref mut payload) => {
            payload.prev_randao = get_byte_vector_32_for_hex(
                "0x9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5",
            );
        }
        ExecutionPayload::Bellatrix(ref mut payload) => {
            payload.prev_randao = get_byte_vector_32_for_hex(
                "0x9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5",
            );
        }
        _ => panic!("unexpected execution payload type"),
    }
    signed_bid_submission.message_mut().proposer_public_key =
        get_valid_payload_register_validator(None).entry.registration.message.public_key;

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .json(&signed_bid_submission)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Prev randao mismatch. got: 0x9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5, expected: 0x9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e4");

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_submit_withdrawal_root_mismatch() {
    // Start the server
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send slot & payload attributes updates
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(
        slot_update_sender.clone(),
        Some(CAPELLA_FORK_EPOCH * SLOTS_PER_EPOCH),
        Some(CAPELLA_FORK_EPOCH * SLOTS_PER_EPOCH + 1),
    )
    .await;
    send_dummy_payload_attributes_update(
        slot_update_sender,
        Some(CAPELLA_FORK_EPOCH * SLOTS_PER_EPOCH + 1),
    )
    .await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let mut signed_bid_submission: SignedBidSubmission = load_bid_submission_from_file(
        "submitBlockPayloadCapella_Goerli_incorrect_withdrawal_root.json",
        Some(CAPELLA_FORK_EPOCH * SLOTS_PER_EPOCH + 1),
        Some(1681338467),
    );
    signed_bid_submission.message_mut().proposer_public_key =
        get_valid_payload_register_validator(None).entry.registration.message.public_key;

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .json(&signed_bid_submission)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_submit_block_max_payload_length_exceeded() {
    // Start the server
    let (tx, http_config, _api, _slot_update_receiver) = start_api_server().await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let my_vec = vec![0u8; MAX_PAYLOAD_LENGTH + 1];

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .body(my_vec)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(resp.text().await.unwrap(), "length limit exceeded");

    // Shut down the server
    let _ = tx.send(());
}

// TODO: fix this test. This test no longer works as we now check the signature
// before we sanity check
#[tokio::test]
#[ignore]
#[serial]
async fn test_submit_block_zero_value_block() {
    // Start the server
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send slot & payload attributes updates
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let signed_bid_submission: SignedBidSubmission = load_bid_submission_from_file(
        "submitBlockPayloadCapella_Goerli_zero_value.json",
        None,
        None,
    );

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .json(&signed_bid_submission)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Zero value block");

    // Shut down the server
    let _ = tx.send(());
}

// TODO: fix this test. This test no longer works as we now check the signature
// before we sanity check
#[tokio::test]
#[ignore]
#[serial]
async fn test_submit_block_empty_transactiions() {
    // Start the server
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send slot & payload attributes updates
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let signed_bid_submission: SignedBidSubmission = load_bid_submission_from_file(
        "submitBlockPayloadCapella_Goerli_empty_transactions.json",
        None,
        None,
    );

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .json(&signed_bid_submission)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Zero value block");

    // Shut down the server
    let _ = tx.send(());
}

// TODO: fix this test. This test no longer works as we now check the signature
// before we sanity check
#[tokio::test]
#[ignore]
#[serial]
async fn test_submit_block_incorrect_block_hash() {
    // Start the server
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send slot & payload attributes updates
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let signed_bid_submission: SignedBidSubmission = load_bid_submission_from_file(
        "submitBlockPayloadCapella_Goerli_incorrect_block_hash.json",
        None,
        None,
    );

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .json(&signed_bid_submission)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Block hash mismatch. message: 0x2bafdc454116b605005364976b134d761dd736cb4788d25c835783b46daeb121, payload: 0x1bafdc454116b605005364976b134d761dd736cb4788d25c835783b46daeb121");

    // Shut down the server
    let _ = tx.send(());
}

// TODO: fix this test. This test no longer works as we now check the signature
// before we sanity check
#[tokio::test]
#[ignore]
#[serial]
async fn test_submit_block_incorrect_parent_hash() {
    // Start the server
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send slot & payload attributes updates
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let signed_bid_submission: SignedBidSubmission = load_bid_submission_from_file(
        "submitBlockPayloadCapella_Goerli_incorrect_parent_hash.json",
        None,
        None,
    );

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .json(&signed_bid_submission)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Parent hash mismatch. message: 0xbd3291854dc822b7ec585925cda0e18f06af28fa2886e15f52d52dd4b6f94ed6, payload: 0xcd3291854dc822b7ec585925cda0e18f06af28fa2886e15f52d52dd4b6f94ed6");

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_housekeep() {
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send a slot update
    // wait for the slot update to be received
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // GET validators
    let req_url = format!("{}{}", http_config.base_url(), Route::GetValidators.path());
    let resp = reqwest::Client::new().get(req_url.as_str()).send().await.unwrap();

    // Check the response
    assert_eq!(resp.status(), reqwest::StatusCode::OK); // proposer duty bytes is set

    // assert the body is the bytes of the new duties
    let body = resp.bytes().await.unwrap();
    let expected_response = get_dummy_slot_update(None, None).new_duties.unwrap();
    let expected_response: Vec<BuilderGetValidatorsResponse> =
        expected_response.into_iter().map(|item| item.into()).collect();

    let expected_json_bytes = serde_json::to_string(&expected_response).unwrap();

    assert_eq!(body, expected_json_bytes);

    // Test payload attributes is updated
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());
    let mut signed_bid_submission: SignedBidSubmission = load_bid_submission();
    match signed_bid_submission.execution_payload_mut() {
        ExecutionPayload::Capella(ref mut payload) => {
            payload.prev_randao = get_byte_vector_32_for_hex(
                "0x9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5",
            );
        }
        ExecutionPayload::Bellatrix(ref mut payload) => {
            payload.prev_randao = get_byte_vector_32_for_hex(
                "0x9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5",
            );
        }
        _ => panic!("unexpected execution payload type"),
    }

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .json(&signed_bid_submission)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Proposer public key mismatch. got: 0x8559727ee65c295279332198029c939557f4d2aba0751fc55f71d0733b8aa17cd0301232a7f21a895f81eacf55c97ec4, expected: 0x84e975405f8691ad7118527ee9ee4ed2e4e8bae973f6e29aa9ca9ee4aea83605ae3536d22acc9aa1af0545064eacf82e");

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_submit_block_timeout_triggered() {
    // Start the server
    let (tx, http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send slot & payload attributes updates
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let cancellations_enabled = false;
    let req_url = format!(
        "{}{}{}",
        http_config.base_url(),
        Route::SubmitBlock.path(),
        if cancellations_enabled { "?cancellations=1" } else { "" }
    );

    let mut body: FuturesOrdered<
        Pin<Box<dyn Future<Output = Result<Vec<u8>, Infallible>> + Send>>,
    > = FuturesOrdered::new();
    body.push_back(Box::pin(async move {
        // Stream max allowed payload length bytes
        Ok::<_, Infallible>(vec![0u8; MAX_PAYLOAD_LENGTH])
    }));
    body.push_back(Box::pin(async move {
        // never complete the request
        pending::<()>().await;
        Ok::<_, Infallible>(vec![])
    }));
    let body = Body::wrap_stream(body);

    let test_timeout = API_REQUEST_TIMEOUT + Duration::from_secs(3);
    let timeout_result = tokio::time::timeout(test_timeout, async {
        // Send request by streaming the malicious body
        let resp = reqwest::Client::new()
            .post(req_url.as_str())
            .header("accept", "*/*")
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::REQUEST_TIMEOUT);

        // Shut down the server
        let _ = tx.send(());
    })
    .await;

    // Check if the test timed out
    if timeout_result.is_err() {
        panic!("Test timed out");
    }
}

#[tokio::test]
#[serial]
async fn websocket_test() {
    let (tx, _http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send a slot update
    // wait for the slot update to be received
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    //let req_url = "ws://relay.ultrasound.money/ws/v1/top_bid";
    //let req_url = "ws://holesky.titanrelay.xyz/relay/v1/builder/top_bid";

    let req_url = format!("{}{}", "ws://localhost:3000", Route::GetTopBid.path(),);

    let request = tungstenite::http::Request::builder()
        .uri(req_url)
        .header("X-api-key", "valid")
        .body(())
        .unwrap();

    // Connect to the server
    let (mut ws_stream, _) = connect_async(request).await.expect("Failed to connect");

    let mut message_count = 0;
    // Read messages from the server
    while let Some(message) = ws_stream.next().await {
        match message {
            Ok(msg) => {
                match msg {
                    Message::Binary(msg) => {
                        let payload: TopBidUpdate =
                            ethereum_consensus::ssz::prelude::deserialize(&msg).unwrap();
                        assert_eq!(payload.slot, 0);
                    }
                    Message::Text(_msg) => {}
                    Message::Ping(_) => {
                        ws_stream.send(Message::Pong(vec![])).await.unwrap();
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => {}
                }

                message_count += 1;
                if message_count >= 3 {
                    break
                }
            }
            Err(e) => {
                println!("Error: {}", e);
                break
            }
        }
    }

    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn websocket_test_auth_fails() {
    let (tx, _http_config, _api, mut slot_update_receiver) = start_api_server().await;

    // Send a slot update
    // wait for the slot update to be received
    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    //let req_url = "ws://relay.ultrasound.money/ws/v1/top_bid";
    //let req_url = "ws://holesky.titanrelay.xyz/relay/v1/builder/top_bid";

    let req_url = format!("{}{}", "ws://localhost:3000", Route::GetTopBid.path(),);

    let request = tungstenite::http::Request::builder()
        .uri(req_url)
        .header("X-api-key", "invalid")
        .body(())
        .unwrap();

    // Connect to the server
    let result = connect_async(request).await;
    assert!(result.is_err());
    assert_eq!(result.err().unwrap().to_string(), "HTTP error: 401 Unauthorized");

    let _ = tx.send(());
}

#[tokio::test]
async fn test_calculate_withdrawals_root() {
    let json_str = r#"
        [
            {"index": "53516667", "validator_index": "226593", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18829431"},
            {"index": "53516668", "validator_index": "226594", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18895995"},
            {"index": "53516669", "validator_index": "226595", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18921948"},
            {"index": "53516670", "validator_index": "226596", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18879996"},
            {"index": "53516671", "validator_index": "226597", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18862058"},
            {"index": "53516672", "validator_index": "226598", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18877682"},
            {"index": "53516673", "validator_index": "226599", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18876362"},
            {"index": "53516674", "validator_index": "226600", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18905370"},
            {"index": "53516675", "validator_index": "226601", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18911572"},
            {"index": "53516676", "validator_index": "226602", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18908681"},
            {"index": "53516677", "validator_index": "226603", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18904531"},
            {"index": "53516678", "validator_index": "226604", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18829228"},
            {"index": "53516679", "validator_index": "226605", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18883181"},
            {"index": "53516680", "validator_index": "226606", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "63784192"},
            {"index": "53516681", "validator_index": "226607", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18875686"},
            {"index": "53516682", "validator_index": "226608", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18924753"}
        ]
    "#;

    let withdrawals: Vec<Withdrawal> = serde_json::from_str(json_str).unwrap();

    let withdrawals_root = calculate_withdrawals_root(&withdrawals);

    // calculate_withdrawals_root use MPT to calculate the root
    assert_eq!(
        hex::encode(withdrawals_root),
        "25068b16d9a849006edff1fbe9bf96799ef524f0ba87199559d1f714719a8202"
    );

    let mut wlist: List<Withdrawal, 16> = withdrawals.try_into().unwrap();
    let root = wlist.hash_tree_root().unwrap();

    // hash_tree_root use SSZ to calculate the root
    assert_eq!(
        hex::encode(root.deref()),
        "c4726ded906a1d6775eec5e83fa867cffb9f77c6da58b3ceb2e412df971b07f1"
    );
}

#[tokio::test]
async fn test_calculate_tx_root() {
    let txs = [vec![0_u8, 1, 2, 5], vec![0, 1, 2, 5], vec![0, 1, 2, 5]];

    let a: ByteList<1073741824> = txs[0].as_slice().try_into().unwrap();
    let b: ByteList<1073741824> = txs[1].as_slice().try_into().unwrap();
    let c: ByteList<1073741824> = txs[2].as_slice().try_into().unwrap();
    let x = vec![a, b, c];
    let mut txs_list: List<ByteList<1073741824>, 1048576> = x.try_into().unwrap();
    let root = txs_list.hash_tree_root().unwrap();

    println!("{:?}", hex::encode(root.deref()));
}
