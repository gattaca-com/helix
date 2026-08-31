use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use helix_common::simulator::{SszMergedValidationRequest, SszValidationRequest};
use ssz::Encode;
use tower::ServiceExt;

use crate::{
    testing::{ETH, GWEI, signed_transfer},
    validation::{
        server::router,
        tests::{Fixture, blob_bundle_v2},
    },
};

async fn post(fixture: &Fixture, route: &str, body: Vec<u8>) -> (StatusCode, String) {
    let response = router(fixture.validator(), 4)
        .oneshot(Request::post(route).body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn a_valid_submission_is_accepted() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let request = fixture.ssz_request(&built, true);

    let (status, body) = post(&fixture, "/validate", request.as_ssz_bytes()).await;

    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn an_underpaid_submission_is_rejected_with_a_reason() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let mut request = fixture.ssz_request(&built, true);
    let mut submission = fixture.submission(&built);
    submission.message.value = U256::from(ETH);
    request.signed_bid_submission = fixture.encode_submission(submission);

    let (status, body) = post(&fixture, "/validate", request.as_ssz_bytes()).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("proposer payment"), "{body}");
}

#[tokio::test]
async fn the_merged_route_accepts_a_split_payment() {
    let fixture = Fixture::new().await;
    let base = U256::from(ETH / 4);
    let added = U256::from(ETH / 4);
    let txs = vec![
        fixture.proposer_spend(0),
        signed_transfer(
            &fixture.signers[0],
            fixture.chain_id,
            0,
            fixture.proposer,
            base,
            100 * GWEI,
            0,
        ),
        signed_transfer(
            &fixture.signers[2],
            fixture.chain_id,
            0,
            fixture.proposer,
            added,
            100 * GWEI,
            0,
        ),
    ];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let mut submission = fixture.submission(&built);
    submission.message.value = base + added;
    let request = SszMergedValidationRequest {
        apply_blacklist: false,
        registered_gas_limit: 0,
        parent_beacon_block_root: B256::ZERO,
        inclusion_list: Default::default(),
        decoder_params: None,
        signed_bid_submission: fixture.encode_submission(submission),
        base_payment_tx_index: 1,
    };

    let (status, body) = post(&fixture, "/validate_merged", request.as_ssz_bytes()).await;

    assert_eq!(status, StatusCode::OK, "{body}");
}

/// The request's `apply_blacklist` carries the proposer's filtering preference,
/// so the same block gets both answers.
#[tokio::test]
async fn the_request_decides_whether_the_blacklist_applies() {
    let fixture = Fixture::new().await;
    let listed = Address::repeat_byte(0x9a);
    let fixture = fixture.disallow(&[listed]);
    let txs = vec![signed_transfer(
        &fixture.signers[1],
        fixture.chain_id,
        0,
        listed,
        U256::from(1),
        100 * GWEI,
        0,
    )];
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let mut submission = fixture.submission(&built);
    submission.message.value = U256::ZERO;
    let encoded = fixture.encode_submission(submission);

    let mut filtering = fixture.ssz_request(&built, false);
    filtering.apply_blacklist = true;
    filtering.signed_bid_submission = encoded.clone();
    let (status, body) = post(&fixture, "/validate", filtering.as_ssz_bytes()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("blacklisted"), "{body}");

    let mut unfiltered = fixture.ssz_request(&built, false);
    unfiltered.apply_blacklist = false;
    unfiltered.signed_bid_submission = encoded;
    let (status, body) = post(&fixture, "/validate", unfiltered.as_ssz_bytes()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_malformed_body_is_rejected() {
    let fixture = Fixture::new().await;

    let (status, _) = post(&fixture, "/validate", vec![0xde, 0xad, 0xbe, 0xef]).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// A 128 KiB blob crosses the stack several times in a debug build, which
/// overflows tokio's 2 MiB worker stack. Release builds elide the copies.
fn with_large_stack(test: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(test).unwrap().join().unwrap();
}

#[test]
fn a_blobs_bundle_travels_through_the_wire_format() {
    with_large_stack(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(blobs_bundle_wire_format())
    });
}

async fn blobs_bundle_wire_format() {
    let fixture = Fixture::new().await;
    let bundle = blob_bundle_v2(1);
    let built = fixture.blob_block(&bundle);
    let mut submission = fixture.submission(&built);
    submission.message.value = U256::ZERO;
    submission.blobs_bundle = bundle;
    let mut request = fixture.ssz_request(&built, false);
    request.signed_bid_submission = fixture.encode_submission(submission);

    let (status, body) = post(&fixture, "/validate", request.as_ssz_bytes()).await;

    assert_eq!(status, StatusCode::OK, "{body}");
}

#[test]
fn a_refreshed_list_replaces_the_previous_one() {
    let disallow = Arc::new(dashmap::DashSet::new());
    disallow.insert(Address::repeat_byte(0x11));

    let hash = crate::validation::server::refresh_disallow(&disallow, vec![
        "0x2222222222222222222222222222222222222222".into(),
    ]);

    assert!(hash.is_some(), "a changed list must report a new digest");
    assert!(!disallow.contains(&Address::repeat_byte(0x11)), "stale entries must go");
    assert!(disallow.contains(&Address::repeat_byte(0x22)));
}

#[test]
fn an_unchanged_list_reports_no_new_digest() {
    let disallow = Arc::new(dashmap::DashSet::new());
    let entries = vec!["0x2222222222222222222222222222222222222222".to_string()];

    let first = crate::validation::server::refresh_disallow(&disallow, entries.clone())
        .expect("a first list is always new");

    assert!(!first.is_empty());
    assert_eq!(
        crate::validation::server::refresh_disallow(&disallow, entries),
        None,
        "an unchanged list must report nothing"
    );
}
