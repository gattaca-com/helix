use std::sync::Arc;

use alloy_primitives::{Address, B256, U256, keccak256};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use helix_common::{
    api::builder_api::MAX_PAYLOAD_LENGTH,
    blacklist::DisallowListPayload,
    simulator::{SszMergedValidationRequest, SszValidationResponse, TxDetail},
};
use ssz::{Decode, Encode};
use tower::ServiceExt;

use crate::{
    testing::{ETH, GWEI, signed_transfer},
    validation::{
        server::router,
        tests::{Fixture, blob_bundle_v2},
    },
};

async fn post_raw(fixture: &Fixture, route: &str, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let response = router(fixture.validator(), 4)
        .oneshot(Request::post(route).body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, bytes.to_vec())
}

async fn post(fixture: &Fixture, route: &str, body: Vec<u8>) -> (StatusCode, String) {
    let (status, bytes) = post_raw(fixture, route, body).await;
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// The relay accepts submissions up to `MAX_PAYLOAD_LENGTH` and forwards every one of them
/// here, so anything it takes in must not meet a smaller limit on this side. Axum's default
/// body limit is 2MiB, which produced a steady stream of 413s the relay counted as simulator
/// faults.
#[tokio::test]
async fn a_request_over_the_default_body_limit_is_not_refused() {
    let fixture = Fixture::new().await;

    let (status, body) = post(&fixture, "/validate", vec![0u8; 4 * 1024 * 1024]).await;

    assert_ne!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
}

#[tokio::test]
async fn a_request_over_the_relay_submit_limit_is_refused() {
    let fixture = Fixture::new().await;

    let (status, _) = post(&fixture, "/validate", vec![0u8; MAX_PAYLOAD_LENGTH + 1]).await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_valid_submission_is_accepted() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let request = fixture.ssz_request(&built, true);

    let (status, body) = post(&fixture, "/validate", request.as_ssz_bytes()).await;

    assert_eq!(status, StatusCode::OK, "{body}");
}

/// One block covers every shape: a creation (no recipient), a tip-only
/// payment, a direct transfer to the coinbase, and the builder's own payout,
/// where the coinbase spends and its payment saturates to zero.
#[tokio::test]
async fn a_valid_submission_reports_each_transaction() {
    let fixture = Fixture::new().await;
    let coinbase = fixture.signers[0].address();
    let deployer = &fixture.signers[1];
    let direct = &fixture.signers[2];
    let recipient = Address::repeat_byte(0x66);
    let txs = vec![
        fixture.signed_create(deployer, 0),
        signed_transfer(
            deployer,
            fixture.chain_id,
            1,
            recipient,
            U256::from(GWEI),
            100 * GWEI,
            2 * GWEI,
        ),
        signed_transfer(direct, fixture.chain_id, 0, coinbase, U256::from(3 * GWEI), 100 * GWEI, 0),
        signed_transfer(
            &fixture.signers[0],
            fixture.chain_id,
            0,
            fixture.proposer,
            U256::from(ETH / 2),
            100 * GWEI,
            0,
        ),
    ];
    let hashes: Vec<B256> = txs.iter().map(keccak256).collect();
    let built =
        fixture.build_block(fixture.genesis_hash, fixture.genesis_timestamp + 12, txs, Vec::new());
    let request = fixture.ssz_request(&built, true);

    let (status, body) = post_raw(&fixture, "/validate", request.as_ssz_bytes()).await;

    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let response = SszValidationResponse::from_ssz_bytes(&body).unwrap();
    assert_eq!(response.txs, vec![
        TxDetail {
            hash: hashes[0],
            sender: deployer.address(),
            nonce: 0,
            to: None,
            builder_payment: U256::ZERO,
        },
        TxDetail {
            hash: hashes[1],
            sender: deployer.address(),
            nonce: 1,
            to: Some(recipient),
            builder_payment: U256::from(21_000 * 2 * GWEI),
        },
        TxDetail {
            hash: hashes[2],
            sender: direct.address(),
            nonce: 0,
            to: Some(coinbase),
            builder_payment: U256::from(3 * GWEI),
        },
        TxDetail {
            hash: hashes[3],
            sender: coinbase,
            nonce: 0,
            to: Some(fixture.proposer),
            builder_payment: U256::ZERO,
        },
    ]);
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
    let hashes: Vec<B256> = txs.iter().map(keccak256).collect();
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

    let (status, body) = post_raw(&fixture, "/validate_merged", request.as_ssz_bytes()).await;

    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let response = SszValidationResponse::from_ssz_bytes(&body).unwrap();
    let reported: Vec<B256> = response.txs.iter().map(|tx| tx.hash).collect();
    assert_eq!(reported, hashes);
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

    let hash = crate::validation::server::refresh_disallow(
        &disallow,
        DisallowListPayload::Bare(vec!["0x2222222222222222222222222222222222222222".into()]),
    );

    assert!(hash.is_some(), "a changed list must report a new digest");
    assert!(!disallow.contains(&Address::repeat_byte(0x11)), "stale entries must go");
    assert!(disallow.contains(&Address::repeat_byte(0x22)));
}

#[test]
fn an_unchanged_list_reports_no_new_digest() {
    let disallow = Arc::new(dashmap::DashSet::new());
    let entries = vec!["0x2222222222222222222222222222222222222222".to_string()];

    let first = crate::validation::server::refresh_disallow(
        &disallow,
        DisallowListPayload::Bare(entries.clone()),
    )
    .expect("a first list is always new");

    assert!(!first.is_empty());
    assert_eq!(
        crate::validation::server::refresh_disallow(&disallow, DisallowListPayload::Wrapped {
            items: entries
        }),
        None,
        "the wrapped shape yields the same list"
    );
}

/// Decoder params naming a fork, with everything else at its default.
fn params_for(fork: helix_types::ForkName) -> helix_common::decoder::SubmissionDecoderParams {
    helix_common::decoder::SubmissionDecoderParams {
        compression: Default::default(),
        encoding: helix_common::decoder::Encoding::Ssz,
        merge_type: Default::default(),
        is_dehydrated: false,
        with_mergeable_data: false,
        with_adjustments: false,
        mark_all_txs_mergeable: false,
        dehydrated_v2: false,
        merging_v2: false,
        fork_name: fork,
    }
}

#[tokio::test]
async fn a_gloas_validation_request_is_refused() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let mut request = fixture.ssz_request(&built, true);
    request.decoder_params = Some(params_for(helix_types::ForkName::Gloas));

    let (status, body) = post(&fixture, "/validate", request.as_ssz_bytes()).await;

    assert_eq!(
        status,
        StatusCode::NOT_IMPLEMENTED,
        "a Gloas block must not be validated under Fulu rules: {body}",
    );
}

#[tokio::test]
async fn a_merged_gloas_request_is_refused() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let base = fixture.ssz_request(&built, true);
    let request = SszMergedValidationRequest {
        apply_blacklist: base.apply_blacklist,
        registered_gas_limit: base.registered_gas_limit,
        parent_beacon_block_root: base.parent_beacon_block_root,
        inclusion_list: base.inclusion_list,
        decoder_params: Some(params_for(helix_types::ForkName::Gloas)),
        signed_bid_submission: base.signed_bid_submission,
        base_payment_tx_index: 0,
    };

    let (status, body) = post(&fixture, "/validate_merged", request.as_ssz_bytes()).await;

    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "the merged route has the same hole: {body}");
}

#[tokio::test]
async fn a_fulu_request_is_still_validated() {
    let fixture = Fixture::new().await;
    let built = fixture.build_on(fixture.genesis_hash, fixture.genesis_timestamp + 12, 0);
    let mut request = fixture.ssz_request(&built, true);
    request.decoder_params = Some(params_for(helix_types::ForkName::Fulu));

    let (status, body) = post(&fixture, "/validate", request.as_ssz_bytes()).await;

    assert_eq!(status, StatusCode::OK, "{body}");
}

#[test]
fn refusing_an_unsupported_fork_cannot_demote_a_builder() {
    // `SimulatorClient::ssz_request` maps 400 to `BlockValidationFailed`, which
    // demotes, and everything else to `RpcError`, which does not. Refusing a
    // fork is helix's own limitation, so it must not cost a builder its
    // optimistic status.
    assert_ne!(StatusCode::NOT_IMPLEMENTED, StatusCode::BAD_REQUEST);
    assert!(!helix_common::simulator::BlockSimError::RpcError.is_demotable());
}
