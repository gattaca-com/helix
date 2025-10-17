use core::panic;
use std::{
    convert::Infallible, future::pending, io::Write, pin::Pin, str::FromStr, sync::Arc,
    time::Duration,
};

use alloy_primitives::{address, b256, hex, B256, U256};
use axum::http::{header, Method, Request, Uri};
use futures::{stream::FuturesOrdered, Future, SinkExt, StreamExt};
use helix_beacon::types::PayloadAttributes;
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
use helix_database::mock_database_service::MockDatabaseService;
use helix_datastore::MockAuctioneer;
use helix_housekeeper::{CurrentSlotInfo, PayloadAttributesUpdate, SlotUpdate};
use helix_types::{
    get_fixed_pubkey, get_fixed_secret, BlsPublicKey, BlsSignature, ChainSpec, ExecutionPayloadRef,
    ForkName, SignedBidSubmission, SignedBidSubmissionElectra, SignedRoot,
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
        SignedBidSubmission::Electra(ref mut sub) => {
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
async fn test_signed_bid_submission_decoding_electra_gzip() {
    let bytes = b"x\x04\x01\0\0\0\0\0\x06_*\xc7!\xad\xeb\xfdI\x8b\xe6\x89\x81\xae,\xe0\x96\xf1\xeb\x13\xc4\xe40|\x0b\x13\xb1Mf-\x96\n\xbc\xb1Be\x94.\xab\x8f\x91G\xbb\xb1\xb0\xe5\x81\xb7^X+\x08\xa1\xde\xc9\x9d\x9aK\x9b\x99Yx\xef\xae\xa9zn-S4\xf8\x89\n\xbf\x9fH\xc0n\xd8\xd6\xb98!\xcf&\xd8\x9c/VM\xe6\xa0 \x95\x8a\x9a\xca\x14v{\xc7\x90\xe1Y\x85Eij\xf6\xe7S\x88\xb9\xefIA)\xdf\xe9%A\xa1`}\xdey\x02_\xec\xe5\xc7\x01\x9fZ3T:aO\xdb\xea\x87\x17Xh]\x12\xc8\x06&\xd9\t\xec\xe9\0\xd5B\xeeqT\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30\0Q%\x02\0\0\0\0}\xd06\x01\0\0\0\0\x8b\x14l\x84\xb3{\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0X\x01\0\0s\r\0\0\x7f\r\0\0\x93\xe5\xfd\x8a\x1d\x0f\x06\xe8\xe9@\xa2'\xb5\xbdPI\xcf|\xa9\xdeF\xe2\xd4!\xdb\x80;\xa0\x1e'S\xdf!\x13\xdf[\x8e\xf3\x868&\x07\x7f\xab\x82\xe5IP\x07\xa9\xe1\x9c\xa5\xc4\x02\xc2[\xd8\x80\xe4\x07\xa7\xb6\x99\x0f\xb2/\xaa\x05\x89I\x81hk\xf5:5\xd7\xd7\x19\x15\xf4\x8f\xe7Y\xeb\xa6\x8c%\xa1\xd87\xb2\xbekH\x06_*\xc7!\xad\xeb\xfdI\x8b\xe6\x89\x81\xae,\xe0\x96\xf1\xeb\x13\xc4\xe40|\x0b\x13\xb1Mf-\x96\nVw~\xd8>\xea\x1f]\xf3\x16,\xf1\x01\xe8\x852\xcet\xb5\x1e\xe3\xbaL)\xb5\xda|\r\xf8\x8b\xbe\xea\xd2\x94\x0eBJ\"_\x93\x05+\xf4\x83\x1e\xe6\xaa\xa9\x08\xb16\xddb\xbdS\xdf\xc5\xc6yqL/m\xd1%\x14\xad\x1c\xa0\xb2\xc8\xee\x04\xce\x18U\xe1u\xc5\xf8\x0e\xc2\xf1W\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\xe7\xd7\xb56\xfc\xb3TPoG\xaf\xf3\x04\x04\xe2\xab\xf0\xf2\xc8l\x89\x05\xfb\x81/@\0!\x97\xab\x1a\x9c/\xf0\0\0\0\0\0\0\0Q%\x02\0\0\0\0}\xd06\x01\0\0\0\0\xb8F\xe4g\0\0\0\0\x10\x02\0\0I|\xa56\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\xbc\xb1Be\x94.\xab\x8f\x91G\xbb\xb1\xb0\xe5\x81\xb7^X+\x08\xa1\xde\xc9\x9d\x9aK\x9b\x99Yx\xef\xae\x14\x02\0\0[\t\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\xf0\x9f\x8c\x8a(\0\0\0\x1c\x02\0\0\xb3\x02\0\0J\x03\0\0\xe1\x03\0\0x\x04\0\0\x0f\x05\0\0\xa6\x05\0\0=\x06\0\0\xd4\x06\0\0\xf9\x01\xf1\x80\x84~l\xb5\xf0\x83\x02y\"\x80\x80\xb9\x01\x9c`\x80`@R4\x80\x15a\0\x10W`\0\x80\xfd[Pa\x01|\x80a\0 `\09`\0\xf3\xfe`\x80`@R4\x80\x15a\0\x10W`\0\x80\xfd[P`\x046\x10a\0+W`\05`\xe0\x1c\x80cSW\x94C\x14a\00W[`\0\x80\xfd[a\08a\0NV[`@Qa\0E\x91\x90a\0\xc4V[`@Q\x80\x91\x03\x90\xf3[```@Q\x80`@\x01`@R\x80`\n\x81R` \x01\x7fjgqspelekh\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x81RP\x90P\x90V[`\0a\0\x96\x82a\0\xe6V[a\0\xa0\x81\x85a\0\xf1V[\x93Pa\0\xb0\x81\x85` \x86\x01a\x01\x02V[a\0\xb9\x81a\x015V[\x84\x01\x91PP\x92\x91PPV[`\0` \x82\x01\x90P\x81\x81\x03`\0\x83\x01Ra\0\xde\x81\x84a\0\x8bV[\x90P\x92\x91PPV[`\0\x81Q\x90P\x91\x90PV[`\0\x82\x82R` \x82\x01\x90P\x92\x91PPV[`\0[\x83\x81\x10\x15a\x01 W\x80\x82\x01Q\x81\x84\x01R` \x81\x01\x90Pa\x01\x05V[\x83\x81\x11\x15a\x01/W`\0\x84\x84\x01R[PPPPV[`\0`\x1f\x19`\x1f\x83\x01\x16\x90P\x91\x90PV\xfe\xa2dipfsX\"\x12 e\xc9+\x992\x14\xa1\x8f\xa7t\x88\xe7\x95\xae\x8a\x8a\xd8D\xa5\xb2\x8d\x19\xed=\xee\x06\x83\x9f\x08;`\xfadsolcC\0\x08\0\03\x83\x11\x17\x84\xa0\xceB\xc4W\x91R\xce\x05l\xb7\x8c\xf2:\xda\xf2\xca?\xad?'\xf1\x89\xb0\xad\x02:\xbe\xa5\x160\xa8.\xa0V\xb8N\xc2\xcb~\xb9\xd1I\xa5:\xa6\xd2\x17\xcc\xa9\x08U5\x8a\xadq\xfe.N\x80\x8f\xf9%N\xb1>\x02\xf8\x94\x83\x08\x8b\xb0\x82\x1dl\x84;\x9a\xca\0\x84;\x9a\xca\0\x83&\xe0\xbf\x940\xaef\xdcQ\xb2\t\xdf\xee\xfeO\x97\xca\x9b\x14\xb5;\x95Kn\x80\xa4AXsY\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0&%\xa0\xc0\x80\xa0^\x88M\x07\xd1K\xd4R\xabW\xe7j|\xa3\xae)\x05\x1e\x03\x1eG\xb1\xe0\xac\xab\x1b\xc7\xf2N\xea\xa8\x84\xa0D_\xa5\x11S`\xf9\x9b^\xdf\xa3\x0ce\x92\xea'\x14r\xe5\x8b`\xfd,\x1c#p3\x9bN\xbdg\xcb\x02\xf8\x94\x83\x08\x8b\xb0\x82\x1dc\x84;\x9a\xca\0\x84;\x9a\xca\0\x83&\xe0\xbf\x940\xaef\xdcQ\xb2\t\xdf\xee\xfeO\x97\xca\x9b\x14\xb5;\x95Kn\x80\xa4AXsY\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0&%\xa0\xc0\x80\xa0\x83\xd1\xdb\x91\xed)\xedm\xd4\x18z\xd8?\xb9n3\xe6\x04\xe0\x12E\x83\xbd\xf3\xf6\xdb\xc5&'\x063\xb8\xa0\x11\xf2\x88\x9b\xcd\x10aU\xe1\xcbB$\x11Aw\xa5p\x82\xef\xb0\xf4\xf2\xe1\xfd\x02\xde\xfb\xff/\x82\x19\x9b\x02\xf8\x94\x83\x08\x8b\xb0\x82\x1d]\x84;\x9a\xca\0\x84;\x9a\xca\0\x83&\xe0\xbf\x940\xaef\xdcQ\xb2\t\xdf\xee\xfeO\x97\xca\x9b\x14\xb5;\x95Kn\x80\xa4AXsY\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0&%\xa0\xc0\x80\xa0i]\x02\x9aj\x8e\xcd\x13\xe0c\x9c\x13\x12\x93\x93\x0e\xde\x86T\xdf\xce/\xb8v\x87\xd5\x0c\xf7J\x0c\xd3[\xa0\x0f\x91Q\x05yq\xa3\xed\xa7\x1b\xc5\xc8\x83\x17p#\xd0Q\x18\x02\xd6a\x05\x89\x08\x8c\x95z\x8e\xad\xfe9\x02\xf8\x94\x83\x08\x8b\xb0\x82\x1dZ\x84;\x9a\xca\0\x84;\x9a\xca\0\x83&\xe0\xbf\x940\xaef\xdcQ\xb2\t\xdf\xee\xfeO\x97\xca\x9b\x14\xb5;\x95Kn\x80\xa4AXsY\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0&%\xa0\xc0\x80\xa0\x9a\xa5\x8c\xfe\x15\xf6\x98I\x05G\xc8\x11l\xd1\xcdS\xf4\xb9\xd0\xed\xaa\x1b\xa16\x05\x97c\x07\x01\x93B\xd4\xa0z\xd8\xb2+\x9fG\xd6*\xaf\x05\x0e\xab\xe4\x90{\xf3D\xa2\x96x+y\xc5\xa6\x93Rz\xa2\x9a\xc8)c\x02\xf8\x94\x83\x08\x8b\xb0\x82\x1d\\\x84;\x9a\xca\0\x84;\x9a\xca\0\x83&\xe0\xbf\x940\xaef\xdcQ\xb2\t\xdf\xee\xfeO\x97\xca\x9b\x14\xb5;\x95Kn\x80\xa4AXsY\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0&%\xa0\xc0\x01\xa0-\xf5\x90N]\xea\xa3\x81@\xbb\x99\xb5\xc0\x95:H\xac\x1e^\x11\xb1\x1a\x12\x0b\xca\0Y\x11\xbc9\xcd\xa5\xa0>y,\x1dw509\xa3s\xb6\x8a+Qe\x84\xf9f\x12m\xa34x\x12\xbe\x06Dm/\xbdb\xb0\x02\xf8\x94\x83\x08\x8b\xb0\x82\x1da\x84;\x9a\xca\0\x84;\x9a\xca\0\x83&\xe0\xbf\x940\xaef\xdcQ\xb2\t\xdf\xee\xfeO\x97\xca\x9b\x14\xb5;\x95Kn\x80\xa4AXsY\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0&%\xa0\xc0\x80\xa0\xcf\xf6<\xf9\xd4\xc0\xbd\nr\xb4w\0\xe8sT\x99\xf7\xa9\x0evf\xfc\xb8\x1eV\x96\xf3\xbc;\xfc\xb4,\xa0qJ\xff\x03\x1e\xce\xa3\xbb\xe7\x18\x8avO\">\xbd\x03KK\x93\xf0\xb2\x0bOL\x01A\xf1s\x80\xb8W\x02\xf8\x94\x83\x08\x8b\xb0\x82\x1d_\x84;\x9a\xca\0\x84;\x9a\xca\0\x83&\xe0\xbf\x940\xaef\xdcQ\xb2\t\xdf\xee\xfeO\x97\xca\x9b\x14\xb5;\x95Kn\x80\xa4AXsY\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0&%\xa0\xc0\x01\xa0\xfacd\xb7\xb27\x14\xd6Ug\x89\x94\xa7\xd1~\x89Mv\r\xcfnSb)9\x83k;QG\xede\xa0$\x9f<\xc9\xf3^\xa4\xad\xd3,\x0b\xc9W\xd6W\xe6\xf9\xd3e\x80\x9eR\n\xfd6\xbf\x9bX,4\xfe\x9f\x02\xf8\x94\x83\x08\x8b\xb0\x82\x1dX\x84;\x9a\xca\0\x84;\x9a\xca\0\x83&\xe0\xbf\x940\xaef\xdcQ\xb2\t\xdf\xee\xfeO\x97\xca\x9b\x14\xb5;\x95Kn\x80\xa4AXsY\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0&%\xa0\xc0\x80\xa0\xe2\x11\x9d\xcf\x0c\x96\xda4\x82\xf3\x1dWh\xab<x\xe2,\x96\x1e<A}\xf8\x9b\xd7W\x18\xd0\x87\x06 \xa0+0\x82\x90\xc6S\xefr\x80Z\xc3\xa6&TMj)\xa1\xfajkm\xddW}\xcd\x1cO:${\x86\x02\xf8p\x83\x08\x8b\xb0\x10\x80\x846\xa5|I\x82R\x08\x94\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30\x87\x06{\xb3\x84l\x14\x8b\x80\xc0\x80\xa0\x85\x80\xe3\xe9\xe4\x11\x10\xa5\xe7/a\x07\xe0\xcf%\x1a^\xec\xf8\xa2Y\xc1\xff\xbdZa\xbe\xbc\xf7\xcb\x88\xd7\xa0Xn\xb3\x7f\xca@\xd7\x03\x08\xc2\xd0\0\xf1\x08\xe9\xcb\xbb\xce\xde\xad\xa6\xec\xc3\xa0\xe3\x81S\xe8\x90.\x9bG\x86\xd2\x0e\0\0\0\0\0(a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30\x10\xc6\x16\0\0\0\0\0\x87\xd2\x0e\0\0\0\0\0)a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30U'\x17\0\0\0\0\0\x88\xd2\x0e\0\0\0\0\0*a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30Z\xff\x16\0\0\0\0\0\x89\xd2\x0e\0\0\0\0\0+a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30\xa1\xcf\x16\0\0\0\0\0\x8a\xd2\x0e\0\0\0\0\0,a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30F\xe8\x16\0\0\0\0\0\x8b\xd2\x0e\0\0\0\0\0-a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30\\\xe9\x16\0\0\0\0\0\x8c\xd2\x0e\0\0\0\0\0.a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30\xa8\xfd\x16\0\0\0\0\0\x8d\xd2\x0e\0\0\0\0\0/a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc302\x08\x17\0\0\0\0\0\x8e\xd2\x0e\0\0\0\0\00a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc309\xef\x16\0\0\0\0\0\x8f\xd2\x0e\0\0\0\0\01a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30E\x02\x17\0\0\0\0\0\x90\xd2\x0e\0\0\0\0\02a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30\xe7\xc5\x16\0\0\0\0\0\x91\xd2\x0e\0\0\0\0\03a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30\x15\xe6\x16\0\0\0\0\0\x92\xd2\x0e\0\0\0\0\04a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30\xfe\xdc\x16\0\0\0\0\0\x93\xd2\x0e\0\0\0\0\05a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30\x05\0\x17\0\0\0\0\0\x94\xd2\x0e\0\0\0\0\06a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30%\x17\x17\0\0\0\0\0\x95\xd2\x0e\0\0\0\0\07a\x0f\0\0\0\0\0\xe5\x06\x1f\xe5\xb4\xd0\xbd&\x0f\x1f\xf8\x0f\xa9\x19\xe39\xf1\xf5\xc30\xd2\xe5\x16\0\0\0\0\0\x0c\0\0\0\x0c\0\0\0\x0c\0\0\0\x0c\0\0\0\x0c\0\0\0\x0c\0\0\0";
    assert!(SignedBidSubmissionElectra::from_ssz_bytes(bytes).is_ok());
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
        SignedBidSubmission::Electra(bid) => bid.signature = BlsSignature::test_random(),
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
