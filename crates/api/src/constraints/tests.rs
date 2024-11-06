// +++ IMPORTS +++

use std::{sync::Arc, time::Duration};

use ethereum_consensus::{
    builder::ValidatorRegistration,
    primitives::{BlsPublicKey, BlsSignature},
    ssz,
};
use helix_common::{
    api::{
        builder_api::BuilderGetValidatorsResponseEntry,
        constraints_api::{SignedDelegation, SignedRevocation, MAX_CONSTRAINTS_PER_SLOT},
        proposer_api::ValidatorRegistrationInfo,
    },
    bellatrix::{ByteVector, List},
    deneb::SignedValidatorRegistration,
    proofs::SignedConstraints,
    Route, ValidatorPreferences,
};
use helix_database::MockDatabaseService;
use helix_datastore::MockAuctioneer;
use helix_housekeeper::{ChainUpdate, SlotUpdate};
use helix_utils::request_encoding::Encoding;
use rand::Rng;
use reqwest::{Client, Response};
use reth_primitives::hex;
use serial_test::serial;
use tokio::sync::{
    mpsc::{Receiver, Sender},
    oneshot,
};
use tracing::info;

use crate::{
    builder::{api::BuilderApi, mock_simulator::MockSimulator},
    constraints::api::ConstraintsApi,
    gossiper::mock_gossiper::MockGossiper,
    test_utils::constraints_api_app,
};

// +++ HELPER VARIABLES +++
const ADDRESS: &str = "0.0.0.0";
const PORT: u16 = 3000;
const HEAD_SLOT: u64 = 32; //ethereum_consensus::configs::mainnet::CAPELLA_FORK_EPOCH;
const SUBMISSION_SLOT: u64 = HEAD_SLOT + 1;
const SUBMISSION_TIMESTAMP: u64 = 1606824419;
const VALIDATOR_INDEX: usize = 1;
#[allow(dead_code)]
const SECRET_KEY: &str = "0x0e8e05025f246b4585d6c95f37d9286ea54bafb6e98de6554e8063084424f91b";
const PUBLIC_KEY: &str = "0x87001c7c7546c2880b93629c685f8fad3aacdd7d5e59303996fad07c2cc135413eb0568ec84e7594719127827c2717a1";

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

fn get_test_pub_key_bytes(random: bool) -> [u8; 48] {
    if random {
        let mut pubkey_array = [0u8; 48];
        rand::thread_rng().fill(&mut pubkey_array[..]);
        pubkey_array
    } else {
        let pubkey_bytes = hex::decode(&PUBLIC_KEY[2..]).unwrap();
        let mut pubkey_array = [0u8; 48];
        pubkey_array.copy_from_slice(&pubkey_bytes);
        pubkey_array
    }
}

fn get_byte_vector_20_for_hex(hex: &str) -> ByteVector<20> {
    let bytes = hex::decode(&hex[2..]).unwrap();
    ByteVector::try_from(bytes.as_ref()).unwrap()
}

fn get_valid_payload_register_validator(
    submission_slot: Option<u64>,
    validator_index: Option<usize>,
) -> BuilderGetValidatorsResponseEntry {
    BuilderGetValidatorsResponseEntry {
            slot: submission_slot.unwrap_or(SUBMISSION_SLOT),
            validator_index: validator_index.unwrap_or(VALIDATOR_INDEX),
            entry: ValidatorRegistrationInfo {
                registration: SignedValidatorRegistration {
                    message: ValidatorRegistration {
                        fee_recipient: get_byte_vector_20_for_hex("0x5cc0dde14e7256340cc820415a6022a7d1c93a35"),
                        gas_limit: 30000000,
                        timestamp: SUBMISSION_TIMESTAMP,
                        public_key: BlsPublicKey::try_from(&get_test_pub_key_bytes(false)[..]).unwrap(),
                    },
                    signature: BlsSignature::try_from(hex::decode(&"0x834646477659587229991c5411aee68cb06d86c71aada0dfd0d0c50c7a75200d82f07d0b91d0f3c30924952f0e3e7ed7094033e82db91e54bbfdf07350a8a397c3af9914b530ecc38e55b4c70559e1bd82d7c7c9d22f4fc3fad612bc67f869f3"[2..]).unwrap().as_slice()).unwrap(),
                },
                preferences: ValidatorPreferences::default(),
            }
        }
}

fn get_dummy_slot_update(
    head_slot: Option<u64>,
    submission_slot: Option<u64>,
    validator_index: Option<usize>,
) -> SlotUpdate {
    SlotUpdate {
        slot: head_slot.unwrap_or(HEAD_SLOT),
        next_duty: Some(get_valid_payload_register_validator(submission_slot, validator_index)),
        new_duties: Some(vec![get_valid_payload_register_validator(
            submission_slot,
            validator_index,
        )]),
    }
}

async fn send_dummy_slot_update(
    slot_update_sender: Sender<ChainUpdate>,
    head_slot: Option<u64>,
    submission_slot: Option<u64>,
    validator_index: Option<usize>,
) {
    let chain_update =
        ChainUpdate::SlotUpdate(get_dummy_slot_update(head_slot, submission_slot, validator_index));
    slot_update_sender.send(chain_update).await.unwrap();

    // sleep for a bit to allow the api to process the slot update
    tokio::time::sleep(Duration::from_millis(100)).await;
}

async fn send_request(req_url: &str, encoding: Encoding, req_payload: Vec<u8>) -> Response {
    let client = Client::new();
    let request = client.post(req_url).header("accept", "*/*");
    let request = encoding.to_headers(request);

    request.body(req_payload).send().await.unwrap()
}

async fn start_api_server() -> (
    oneshot::Sender<()>,
    HttpServiceConfig,
    Arc<ConstraintsApi<MockAuctioneer, MockDatabaseService>>,
    Arc<BuilderApi<MockAuctioneer, MockDatabaseService, MockSimulator, MockGossiper>>,
    Receiver<Sender<ChainUpdate>>,
) {
    let (tx, rx) = oneshot::channel();
    let http_config = HttpServiceConfig::new(ADDRESS, PORT);
    let bind_address = http_config.bind_address();

    let (router, constraints_api, builder_api, slot_update_receiver) = constraints_api_app();

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

    (tx, http_config, constraints_api, builder_api, slot_update_receiver)
}

fn _get_signed_constraints_json() -> &'static str {
    r#"[
            {
            "message": {
                "pubkey": "0xa695ad325dfc7e1191fbc9f186f58eff42a634029731b18380ff89bf42c464a42cb8ca55b200f051f57f1e1893c68759",
                "slot": 33,
                "top": true,
                "transactions": [
                "0x02f86c870c72dd9d5e883e4d0183408f2382520894d2e2adf7177b7a8afddbc12d1634cf23ea1a71020180c001a08556dcfea479b34675db3fe08e29486fe719c2b22f6b0c1741ecbbdce4575cc6a01cd48009ccafd6b9f1290bbe2ceea268f94101d1d322c787018423ebcbc87ab4"
                ]
            },
            "signature": "0xb8d50ee0d4b269db3d4658c1dac784d273a4160d769e16dce723a9684c390afe5865348416b3bf0f1a4f47098bec9024135d0d95f08bed18eb577a3d8a67f5dc78b13cc62515e280786a73fb267d35dfb7ab46a25ac29bf5bc2fa5b07b3e07a6"
            }
        ]"#
}

fn _get_signed_constraint_conflict_1_json() -> &'static str {
    r#"[
            {
            "message": {
                "pubkey": "0xa695ad325dfc7e1191fbc9f186f58eff42a634029731b18380ff89bf42c464a42cb8ca55b200f051f57f1e1893c68759",
                "slot": 32,
                "top": true,
                "transactions": [
                "0x02f86c870c72dd9d5e883e4d0183408f2382520894d2e2adf7177b7a8afddbc12d1634cf23ea1a71020180c001a08556dcfea479b34675db3fe08e29486fe719c2b22f6b0c1741ecbbdce4575cc6a01cd48009ccafd6b9f1290bbe2ceea268f94101d1d322c787018423ebcbc87ab4",
                "0x02f9017b8501a2140cff8303dec685012a05f2008512a05f2000830249f094843669e5220036eddbaca89d8c8b5b82268a0fc580b901040cc7326300000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000022006292538e66f0000000000000000000000005ba38f2c245618e39f6fa067bf1dec304e73ff3c00000000000000000000000092f0ee29e6e1bf0f7c668317ada78f5774a6cb7f000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000020000000000000000000000003fac6482aee49bf58515be2d3fb58378a8497cc9000000000000000000000000c6cc140787b02ae479a10e41169607000c0d44f6c080a00cf74c45dbe9ee1fb923118ec5ce9db8f88cd651196ed3f9d4f8f2a65827e611a04a6bc1d49a7e18b7c92e8f3614cae116b1832ceb311c81d54b2c87de1545f68f"
                ]
            },
            "signature": "0x97d249cbd4b2a33fab4e8d0b698a1905815375c6eb905356768bc23c90b5f2972e03580116849e3f250e7c650486549d16f090135a109c9dd21360e1ca52cb941fada8bdf0280fe184ec4adba7d9246a6d9fc516bfbe508ad1c21cfc2e169b52"
            }
        ]"#
}

fn _get_signed_constraint_conflict_2_json() -> &'static str {
    r#"[
            {
            "message": {
                "pubkey": "0xa695ad325dfc7e1191fbc9f186f58eff42a634029731b18380ff89bf42c464a42cb8ca55b200f051f57f1e1893c68759",
                "slot": 32,
                "top": true,
                "transactions": [
                "0x02f86c870c72dd9d5e883e4d0183408f2382520894d2e2adf7177b7a8afddbc12d1634cf23ea1a71020180c001a08556dcfea479b34675db3fe08e29486fe719c2b22f6b0c1741ecbbdce4575cc6a01cd48009ccafd6b9f1290bbe2ceea268f94101d1d322c787018423ebcbc87ab4"
                ]
            },
            "signature": "0xb648321b682a445d377c3a11180213f1c21e5a645b2f80fb6ec31a85550d3bb0a0fb1eef806d6403a76ede552f87c8f219e0703d254dc78dca0b4a904794f81c995884dd90648190f4e4c5badc7463314ce57ceb5448767211a3c4af5b860650"
            }
        ]"#
}

fn _get_signed_delegations() -> &'static str {
    r#"
        [{
            "message": 
            {
            "action": 0,
            "validator_pubkey": "0x882c02d0c1c30cf9bb84769fc37bf81a73795be9799156ac3a500fba24ddae4f310b47dc27c08e1acdf395a0d9e5ae6a",
            "delegatee_pubkey": "0xa30e3c596a76f109094afbc16689adab5c03fb575213085d3e3a0766d269a961e28dd909312408866c6d481fc8a93522"
            },
            "signature": "0xb067c33c6b8018086ba0b294e069063d185a01116475caa6e4cf36d08d62422ad68ef83ec0b01b4e13dfd95a914f2ed50301e1bfd945d0339b11a0330b06bd532a8bb9cd8017452e1f44f7c64c1ab4888266e87f99c916c90d5fd95614b0dfc4"
        }]"#
}

fn _get_signed_revocations() -> &'static str {
    r#"
        [{
            "message": {
            "action": 1,
            "validator_pubkey": "0x882c02d0c1c30cf9bb84769fc37bf81a73795be9799156ac3a500fba24ddae4f310b47dc27c08e1acdf395a0d9e5ae6a",
            "delegatee_pubkey": "0xa30e3c596a76f109094afbc16689adab5c03fb575213085d3e3a0766d269a961e28dd909312408866c6d481fc8a93522"
            },
            "signature": "0x90b352ee91ef1e9e6411ff4837a0745547941b593e1104200b208644e247242ac1e352e9318de4b3a54fc5bd29b2dbc015c9c47784edde369303a59187c65a860a04b1114e925fe0dfc38e957068f1dff3bd2825946223222c44f8cc871e1b0e"
        }]
        "#
}

// +++ TESTS +++
#[tokio::test]
#[serial]
#[ignore = "TODO: to fix, we're not adding proposer duties to database"]
async fn test_submit_constraints_conflict() {
    let _ = tracing_subscriber::fmt::try_init();

    // Start the server
    let (tx, http_config, _constraints_api, _builder_api, mut slot_update_receiver) =
        start_api_server().await;

    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None, None).await;

    let test_constraint: List<SignedConstraints, MAX_CONSTRAINTS_PER_SLOT> =
        serde_json::from_str(_get_signed_constraint_conflict_1_json()).unwrap();

    // Submit constraints
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBuilderConstraints.path());

    // Send JSON encoded request
    let resp =
        send_request(&req_url, Encoding::Json, serde_json::to_vec(&test_constraint).unwrap()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let test_constraint: List<SignedConstraints, MAX_CONSTRAINTS_PER_SLOT> =
        serde_json::from_str(_get_signed_constraint_conflict_2_json()).unwrap();

    // Send JSON encoded request
    let resp =
        send_request(&req_url, Encoding::Json, serde_json::to_vec(&test_constraint).unwrap()).await;

    // This will result in a conflict as 2 constraints are submitted with top = true
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
    info!("Response: {:?}", resp);

    // Send shutdown signal
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
#[ignore = "TODO: to fix, we're not adding proposer duties to database"]
async fn test_submit_constraints_and_get_constraints_ok() {
    let _ = tracing_subscriber::fmt::try_init();

    // Start the server
    let (tx, http_config, _constraints_api, _builder_api, mut slot_update_receiver) =
        start_api_server().await;

    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None, None).await;

    let test_constraints: List<SignedConstraints, MAX_CONSTRAINTS_PER_SLOT> =
        serde_json::from_str(_get_signed_constraints_json()).unwrap();

    // Submit constraints
    let req_url = format!("{}{}", http_config.base_url(), Route::SubmitBuilderConstraints.path());

    // Send SSZ encoded request
    let resp =
        send_request(&req_url, Encoding::Ssz, ssz::prelude::serialize(&test_constraints).unwrap())
            .await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // Correct and complete the below
    let slot = 32;

    // Get constraints
    let req_url = format!("{}{}", http_config.base_url(), Route::GetBuilderConstraints.path());

    let resp = reqwest::Client::new()
        .get(req_url)
        .query(&[("slot", slot)])
        .header("accept", "application/json")
        .send()
        .await
        .unwrap();

    // Ensure the response is OK
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // Print the response body
    let body: Vec<SignedConstraints> = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
    info!("Response body: {:?}", body);
    // TODO: clean this
    let constraint = body.first().unwrap().clone();
    let send = test_constraints.first().unwrap().clone();
    assert_eq!(constraint.signature, send.signature);

    // Send shutdown signal
    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_delegate_submission_rights_ok() {
    let _ = tracing_subscriber::fmt::try_init();

    let (tx, http_config, _api, _, _) = start_api_server().await;

    let test_delegation: Vec<SignedDelegation> =
        serde_json::from_str(_get_signed_delegations()).unwrap();

    let req_url = format!("{}{}", http_config.base_url(), Route::DelegateSubmissionRights.path());
    let req_payload = serde_json::to_vec(&test_delegation).unwrap();

    // Send JSON encoded request
    let resp = send_request(&req_url, Encoding::Json, req_payload).await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let _ = tx.send(());
}

#[tokio::test]
#[serial]
#[ignore = "TODO: to fix, we're not adding proposer duties to database"]
async fn test_get_delegations() {
    let _ = tracing_subscriber::fmt::try_init();

    // Start the server
    let (tx, http_config, _constraints_api, _builder_api, mut slot_update_receiver) =
        start_api_server().await;

    let slot_update_sender = slot_update_receiver.recv().await.unwrap();
    send_dummy_slot_update(slot_update_sender.clone(), None, None, None).await;

    let test_delegation: Vec<SignedDelegation> =
        serde_json::from_str(_get_signed_delegations()).unwrap();

    let req_url = format!("{}{}", http_config.base_url(), Route::DelegateSubmissionRights.path());
    let req_payload = serde_json::to_vec(&test_delegation).unwrap();

    // Send JSON encoded request
    let resp = send_request(&req_url, Encoding::Json, req_payload).await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // Get delegations
    let slot = 33;

    let req_url = format!("{}{}", http_config.base_url(), Route::GetBuilderDelegations.path());

    let resp = reqwest::Client::new()
        .get(req_url)
        .query(&[("slot", slot)])
        .header("accept", "application/json")
        .send()
        .await
        .unwrap();

    // Ensure the response is OK
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: Vec<BlsPublicKey> = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
    assert_eq!(test_delegation[0].message.delegatee_pubkey, body.first().unwrap().clone());

    let _ = tx.send(());
}

#[tokio::test]
#[serial]
async fn test_revoke_submission_rights_ok() {
    let _ = tracing_subscriber::fmt::try_init();
    let (tx, http_config, _, _, _) = start_api_server().await;

    let test_revocation: Vec<SignedRevocation> =
        serde_json::from_str(_get_signed_revocations()).unwrap();

    let req_url = format!("{}{}", http_config.base_url(), Route::RevokeSubmissionRights.path());
    let req_payload = serde_json::to_vec(&test_revocation).unwrap();

    println!("Payload: {:?}", req_payload);

    // Send JSON encoded request
    let resp = send_request(&req_url, Encoding::Json, req_payload).await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let _ = tx.send(());
}
