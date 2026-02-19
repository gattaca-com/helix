use core::panic;
use std::{
    convert::Infallible, future::pending, io::Write, pin::Pin, str::FromStr, sync::Arc,
    time::Duration,
};

use alloy_primitives::{address, b256, hex, B256, U256};
use axum::http::{header, Method, Request, Uri};
use futures::{stream::FuturesOrdered, Future, SinkExt, StreamExt};
use crate::beacon::types::PayloadAttributes;
use helix_common::{
    api::{
        builder_api::{
            BuilderGetValidatorsResponse, BuilderGetValidatorsResponseEntry, TopBidUpdate,
        },
        proposer_api::ValidatorRegistrationInfo,
    },
    bid_submission::BidSubmission,
    chain_info::ChainInfo,
    metadata_provider::DefaultMetadataProvider,
    request_encoding::Encoding,
    Route, SubmissionTrace, ValidatorPreferences,
};
use crate::databasemock_database_service::MockDatabaseService;
use helix_datastore::MockAuctioneer;
use helix_housekeeper::{CurrentSlotInfo, PayloadAttributesUpdate, SlotUpdate};
use helix_types::{
    get_fixed_pubkey, get_fixed_secret, BlsPublicKey, BlsSignature, ChainSpec, ExecutionPayloadRef,
    ForkName, SignedBidSubmission, SignedRoot,
    SignedValidatorRegistration, TestRandomSeed, ValidatorRegistration,
};
use reqwest::{Client, Response};
use serial_test::serial;
use ssz::{Decode, Encode};
use tokio::sync::oneshot;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, Message},
};
use tonic::transport::Body;

use crate::{
    builder::{
        api::{decode_header_submission, decode_payload, BuilderApi, MAX_PAYLOAD_LENGTH},
        mock_simulator::MockSimulator,
    },
    gossiper::mock_gossiper::MockGossiper,
    service::API_REQUEST_TIMEOUT,
    test_utils::builder_api_app,
};

const ADDRESS: &str = "0.0.0.0";
const PORT: u16 = 3000;
const HEAD_SLOT: u64 = 32; //ethereum_consensus::configs::mainnet::CAPELLA_FORK_EPOCH;
const SUBMISSION_SLOT: u64 = HEAD_SLOT + 1;
const SUBMISSION_TIMESTAMP: u64 = 1606824419;
const VALIDATOR_INDEX: usize = 1;

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

fn get_valid_payload_register_validator(
    submission_slot: Option<u64>,
) -> BuilderGetValidatorsResponseEntry {
    BuilderGetValidatorsResponseEntry {
            slot: submission_slot.unwrap_or(SUBMISSION_SLOT).into(),
            validator_index: VALIDATOR_INDEX as u64,
            entry: ValidatorRegistrationInfo {
                registration: SignedValidatorRegistration {
                    message: ValidatorRegistration {
                        fee_recipient: address!("abcf8e0d4e9587369b2301d0790347320302cc09"),
                        gas_limit: 30000000,
                        timestamp: SUBMISSION_TIMESTAMP,
                        pubkey: get_fixed_pubkey(0),
                    },
                    signature: BlsSignature::deserialize(hex::decode(&"0xaf12df007a0c78abb5575067e5f8b089cfcc6227e4a91db7dd8cf517fe86fb944ead859f0781277d9b78c672e4a18c5d06368b603374673cf2007966cece9540f3a1b3f6f9e1bf421d779c4e8010368e6aac134649c7a009210780d401a778a5").unwrap().as_slice()).unwrap(),
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
    .into()
}

fn get_dummy_payload_attributes() -> PayloadAttributes {
    PayloadAttributes {
        timestamp: SUBMISSION_TIMESTAMP,
        prev_randao: b256!("cf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2"),
        suggested_fee_recipient: "0xabcf8e0d4e9587369b2301d0790347320302cc09".to_string(),
        withdrawals: vec![],
        parent_beacon_block_root: None,
    }
}

fn get_dummy_payload_attributes_update(submission_slot: Option<u64>) -> PayloadAttributesUpdate {
    PayloadAttributesUpdate {
        slot: submission_slot.unwrap_or(SUBMISSION_SLOT),
        parent_hash: b256!("cf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2"),
        withdrawals_root: b256!("da05bc3e664422b7afec68e9497343e818d8eced24ccc103069c897987d0a6d8"),
        payload_attributes: get_dummy_payload_attributes(),
    }
}

async fn send_dummy_slot_update(
    curr_slot_info: CurrentSlotInfo,
    head_slot: Option<u64>,
    submission_slot: Option<u64>,
) {
    let chain_update = get_dummy_slot_update(head_slot, submission_slot);
    curr_slot_info.handle_new_slot(chain_update, &ChainInfo::for_mainnet());

    // sleep for a bit to allow the api to process the slot update
    tokio::time::sleep(Duration::from_millis(100)).await;
}

async fn send_dummy_payload_attributes_update(
    curr_slot_info: CurrentSlotInfo,
    submission_slot: Option<u64>,
) {
    let chain_update = get_dummy_payload_attributes_update(submission_slot);
    curr_slot_info.handle_new_payload_attributes(chain_update);

    // sleep for a bit to allow the api to process the slot update
    tokio::time::sleep(Duration::from_millis(100)).await;
}

fn load_bid_submission() -> SignedBidSubmission {
    let bytes = include_bytes!("../../test_data/signed-bid-submission-deneb-2.json");
    let mut signed_bid_submission: SignedBidSubmission = serde_json::from_slice(bytes).unwrap();

    // set the slot and timestamp
    signed_bid_submission.message_mut().slot = SUBMISSION_SLOT;
    *signed_bid_submission.execution_payload_mut().timestamp_mut() = SUBMISSION_TIMESTAMP;

    resign_bid_submission(&mut signed_bid_submission);

    signed_bid_submission
}

fn resign_bid_submission(bid: &mut SignedBidSubmission) {
    let kp = get_fixed_secret(0);

    assert_eq!(&kp.public_key(), bid.builder_public_key());

    match bid {
        SignedBidSubmission::Deneb(ref mut sub) => {
            let domain = ChainSpec::mainnet().get_builder_domain();
            let root = sub.message.signing_root(domain);
            let sig = kp.sign(root);

            sub.signature = sig;
        }
    }
}

async fn start_api_server() -> (
    oneshot::Sender<()>,
    HttpServiceConfig,
    Arc<
        BuilderApi<
            MockAuctioneer,
            MockDatabaseService,
            MockSimulator,
            MockGossiper,
            DefaultMetadataProvider,
        >,
    >,
    CurrentSlotInfo,
) {
    let (tx, rx) = oneshot::channel();
    let http_config = HttpServiceConfig::new(ADDRESS, PORT);
    let bind_address = http_config.bind_address();

    let (router, api, slot_update_sender) = builder_api_app();

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

    (tx, http_config, api, slot_update_sender)
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

#[test]
fn test_header_submission_decoding_json_deneb() {
    let req_payload_bytes = include_bytes!("../../test_data/submitBlockPayloadHeaderDeneb.json");
    let decoded_submission: SignedHeaderSubmissionDeneb =
        serde_json::from_slice(req_payload_bytes.as_slice()).unwrap();

    assert_eq!(decoded_submission.message.bid_trace.slot, 5552306);
}

#[tokio::test]
async fn test_header_submission_decoding_ssz_deneb() {
    let req_payload_bytes = include_bytes!("../../test_data/header_v2-deneb.ssz");

    let mut header_submission_trace = HeaderSubmissionTrace::default();

    let request = generate_request(false, false, true, req_payload_bytes.as_slice());
    let decoded_submission =
        decode_header_submission(request, &mut header_submission_trace).await.unwrap();

    assert!(matches!(decoded_submission.0, SignedHeaderSubmission::Deneb(_)));
}

#[tokio::test]
async fn test_signed_bid_submission_decoding_deneb() {
    let req_payload_bytes = include_bytes!("../../test_data/submitBlockPayloadDeneb.json");

    let mut submission_trace = SubmissionTrace::default();
    let request = generate_request(false, false, false, req_payload_bytes.as_slice());
    let (decoded_submission, _) = decode_payload(request, &mut submission_trace).await.unwrap();

    assert_eq!(decoded_submission.message().slot, 5552306);
    assert!(matches!(decoded_submission.execution_payload(), ExecutionPayloadRef::Deneb(_)));
    assert!(matches!(
        decoded_submission.execution_payload().clone_from_ref().fork_name(),
        ForkName::Deneb
    ));
    let payload = decoded_submission.execution_payload();
    assert_eq!(payload.blob_gas_used().unwrap(), 100);
    assert_eq!(payload.excess_blob_gas().unwrap(), 50);
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
    let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

    // Send a slot update
    // wait for the slot update to be received
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
    let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

    // Send slot & payload attributes updates
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
    signed_bid_submission.message_mut().proposer_pubkey =
        get_valid_payload_register_validator(None).entry.registration.message.pubkey;

    match &mut signed_bid_submission {
        SignedBidSubmission::Deneb(bid) => bid.signature = BlsSignature::test_random(),
    }

    // Send JSON encoded request
    let resp =
        send_request(&req_url, Encoding::Json, serde_json::to_vec(&signed_bid_submission).unwrap())
            .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), "Signature verification failed");

    // Send SSZ encoded request
    let resp = send_request(&req_url, Encoding::Ssz, signed_bid_submission.as_ssz_bytes()).await;
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
    let req_payload_bytes = signed_bid_submission.as_ssz_bytes();
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
    let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

    // Send slot & payload attributes updates
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
        address!("1230dde14e7256340cc820415a6022a7d1c93a35");

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
    assert_eq!(resp.text().await.unwrap(), "Fee recipient mismatch. got: 0x1230dde14e7256340cc820415a6022a7d1c93a35, expected: 0xabcf8e0d4e9587369b2301d0790347320302cc09");

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_submit_block_submission_for_past_slot() {
    // Start the server
    let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

    // Send slot & payload attributes updates
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

#[tokio::test]
#[serial]
async fn test_submit_block_unknown_proposer_duty() {
    // Start the server
    let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

    // Send slot & payload attributes updates
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let mut signed_bid_submission: SignedBidSubmission = load_bid_submission();
    signed_bid_submission.message_mut().slot = 31;
    resign_bid_submission(&mut signed_bid_submission);

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
        "Submission for past slot. current slot: 32, submission slot: 31"
    );

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_submit_block_incorrect_timestamp() {
    // Start the server
    let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

    // Send slot & payload attributes updates
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let mut signed_bid_submission: SignedBidSubmission = load_bid_submission();
    *signed_bid_submission.execution_payload_mut().timestamp_mut() = 1;

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
async fn test_submit_block_slot_mismatch() {
    // Start the server
    let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

    // Send slot & payload attributes updates
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
    let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

    // Send slot & payload attributes updates
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let mut signed_bid_submission: SignedBidSubmission = load_bid_submission();
    *signed_bid_submission.execution_payload_mut().prev_randao_mut() =
        b256!("9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5");

    signed_bid_submission.message_mut().proposer_pubkey =
        get_valid_payload_register_validator(None).entry.registration.message.pubkey;

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
    assert_eq!(resp.text().await.unwrap(), "Prev randao mismatch. got: 0x9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5, expected: 0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2");

    // Shut down the server
    let _ = tx.send(());
}

// #[tokio::test]
// #[serial]
// async fn test_submit_withdrawal_root_mismatch() {
//     // Start the server
//     let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

//     // Send slot & payload attributes updates

//     send_dummy_slot_update(
//         slot_update_sender.clone(),
//         Some(CAPELLA_FORK_EPOCH * SLOTS_PER_EPOCH),
//         Some(CAPELLA_FORK_EPOCH * SLOTS_PER_EPOCH + 1),
//     )
//     .await;
//     send_dummy_payload_attributes_update(
//         slot_update_sender,
//         Some(CAPELLA_FORK_EPOCH * SLOTS_PER_EPOCH + 1),
//     )
//     .await;

//     // Prepare the request
//     let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

//     let mut signed_bid_submission: SignedBidSubmission = load_bid_submission_from_file(
//         "submitBlockPayloadCapella_Goerli_incorrect_withdrawal_root.json",
//         Some(CAPELLA_FORK_EPOCH * SLOTS_PER_EPOCH + 1),
//         Some(1681338467),
//     );
//     signed_bid_submission.message_mut().proposer_public_key =
//         get_valid_payload_register_validator(None).entry.registration.message.public_key;

//     // Send JSON encoded request
//     let resp = reqwest::Client::new()
//         .post(req_url.as_str())
//         .header("accept", "*/*")
//         .header("Content-Type", "application/json")
//         .json(&signed_bid_submission)
//         .send()
//         .await
//         .unwrap();

//     assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

//     // Shut down the server
//     let _ = tx.send(());
// }

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

#[tokio::test]
#[serial]
async fn test_submit_block_zero_value_block() {
    // Start the server
    let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

    // Send slot & payload attributes updates
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let mut signed_bid_submission: SignedBidSubmission = load_bid_submission();
    signed_bid_submission.message_mut().value = U256::ZERO;
    resign_bid_submission(&mut signed_bid_submission);

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .json(&signed_bid_submission)
        .send()
        .await
        .unwrap();

    // below floor,
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_submit_block_zero_value() {
    // Start the server
    let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

    // Send slot & payload attributes updates
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let mut signed_bid_submission: SignedBidSubmission = load_bid_submission();

    match signed_bid_submission {
        SignedBidSubmission::Deneb(ref mut deneb_submission) => {
            deneb_submission.execution_payload.transactions = Default::default();
            deneb_submission.message.value = U256::ZERO;
        }
        _ => panic!("Invalid signed bid submission"),
    }

    resign_bid_submission(&mut signed_bid_submission);

    // Send JSON encoded request
    let resp = reqwest::Client::new()
        .post(req_url.as_str())
        .header("accept", "*/*")
        .header("Content-Type", "application/json")
        .json(&signed_bid_submission)
        .send()
        .await
        .unwrap();

    // now we check floor before sanity checks
    assert_eq!(resp.status(), reqwest::StatusCode::ACCEPTED);
    assert_eq!(resp.text().await.unwrap(), "Bid below floor, skipped validation");

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_submit_block_incorrect_block_hash() {
    // Start the server
    let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

    // Send slot & payload attributes updates
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let mut signed_bid_submission: SignedBidSubmission = load_bid_submission();
    signed_bid_submission.message_mut().block_hash = B256::ZERO;
    resign_bid_submission(&mut signed_bid_submission);

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
    assert_eq!(resp.text().await.unwrap(), "bid validation error: block hash mismatch: message: 0x0000000000000000000000000000000000000000000000000000000000000000, payload: 0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2");

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_submit_block_incorrect_parent_hash() {
    // Start the server
    let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

    // Send slot & payload attributes updates
    send_dummy_slot_update(slot_update_sender.clone(), None, None).await;
    send_dummy_payload_attributes_update(slot_update_sender, None).await;

    // Prepare the request
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBlock.path());

    let mut signed_bid_submission: SignedBidSubmission = load_bid_submission();
    signed_bid_submission.message_mut().parent_hash = B256::ZERO;
    resign_bid_submission(&mut signed_bid_submission);

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
    assert_eq!(resp.text().await.unwrap(), "payload attributes not yet known");

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_housekeep() {
    let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

    // Send a slot update
    // wait for the slot update to be received
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
    *signed_bid_submission.execution_payload_mut().prev_randao_mut() =
        b256!("9962816e9d0a39fd4c80935338a741dc916d1545694e41eb5a505e1a3098f9e5");

    let random_pubkey = BlsPublicKey::test_random();
    signed_bid_submission.message_mut().proposer_pubkey = random_pubkey.clone();

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
    assert_eq!(resp.text().await.unwrap(), format!("Proposer public key mismatch. got: {:?}, expected: 0xb34cde46f57a246f10dd73ed8714c665dc187b2888353f0b8676c8790e1599de0e96e2a7d515db99126f8d62b7d44ca1", random_pubkey));

    // Shut down the server
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_submit_block_timeout_triggered() {
    // Start the server
    let (tx, http_config, _api, slot_update_sender) = start_api_server().await;

    // Send slot & payload attributes updates
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
    let (tx, _http_config, _api, slot_update_sender) = start_api_server().await;

    // Send a slot update
    // wait for the slot update to be received
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
                        let payload = TopBidUpdate::from_ssz_bytes(&msg).unwrap();
                        assert_eq!(payload.builder_pubkey, get_fixed_pubkey(0));
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
                    break;
                }
            }
            Err(e) => {
                println!("Error: {}", e);
                break;
            }
        }
    }

    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn websocket_test_auth_fails() {
    let (tx, _http_config, _api, slot_update_sender) = start_api_server().await;

    // Send a slot update
    // wait for the slot update to be received
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

// #[tokio::test]
// async fn test_calculate_withdrawals_root() {
//     let json_str = r#"
//         [
//             {"index": "53516667", "validator_index": "226593", "address":
// "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18829431"},             {"index":
// "53516668", "validator_index": "226594", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f",
// "amount": "18895995"},             {"index": "53516669", "validator_index": "226595", "address":
// "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18921948"},             {"index":
// "53516670", "validator_index": "226596", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f",
// "amount": "18879996"},             {"index": "53516671", "validator_index": "226597", "address":
// "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18862058"},             {"index":
// "53516672", "validator_index": "226598", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f",
// "amount": "18877682"},             {"index": "53516673", "validator_index": "226599", "address":
// "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18876362"},             {"index":
// "53516674", "validator_index": "226600", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f",
// "amount": "18905370"},             {"index": "53516675", "validator_index": "226601", "address":
// "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18911572"},             {"index":
// "53516676", "validator_index": "226602", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f",
// "amount": "18908681"},             {"index": "53516677", "validator_index": "226603", "address":
// "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18904531"},             {"index":
// "53516678", "validator_index": "226604", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f",
// "amount": "18829228"},             {"index": "53516679", "validator_index": "226605", "address":
// "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18883181"},             {"index":
// "53516680", "validator_index": "226606", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f",
// "amount": "63784192"},             {"index": "53516681", "validator_index": "226607", "address":
// "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f", "amount": "18875686"},             {"index":
// "53516682", "validator_index": "226608", "address": "0xb9d7934878b5fb9610b3fe8a5e441e8fad7e293f",
// "amount": "18924753"}         ]
//     "#;

//     let withdrawals: Vec<Withdrawal> = serde_json::from_str(json_str).unwrap();

//     let withdrawals_root = calculate_withdrawals_root(&withdrawals);

//     // calculate_withdrawals_root use MPT to calculate the root
//     assert_eq!(
//         hex::encode(withdrawals_root),
//         "25068b16d9a849006edff1fbe9bf96799ef524f0ba87199559d1f714719a8202"
//     );

//     let mut wlist: List<Withdrawal, 16> = withdrawals.try_into().unwrap();
//     let root = wlist.hash_tree_root().unwrap();

//     // hash_tree_root use SSZ to calculate the root
//     assert_eq!(
//         hex::encode(root.deref()),
//         "c4726ded906a1d6775eec5e83fa867cffb9f77c6da58b3ceb2e412df971b07f1"
//     );
// }
