use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use ethrex_common::types::BlobsBundle as EthrexBlobsBundle;
use helix_common::chain_info::ChainInfo;
use helix_types::{BlsKeypair, BlsPublicKeyBytes, Withdrawals};
use ssz::Decode;

use super::*;
use crate::testing::blob_bundle;

const PROPOSER: Address = Address::repeat_byte(0x77);

fn signing() -> RelaySigningContext {
    RelaySigningContext::new(BlsKeypair::random(), Arc::new(ChainInfo::default()))
}

fn submitter(url: &str) -> Submitter {
    Submitter::new(url, "test-key".to_string(), signing())
}

fn slot_context() -> SlotContext {
    SlotContext {
        slot: 42,
        parent_hash: B256::repeat_byte(0x11),
        parent_block_number: 41,
        timestamp: 1_700_000_000,
        prev_randao: B256::repeat_byte(0xcc),
        withdrawals: Withdrawals::default(),
        parent_beacon_block_root: B256::repeat_byte(0xdd),
        proposer_pubkey: BlsPublicKeyBytes::from([7u8; 48]),
        proposer_fee_recipient: PROPOSER,
        registered_gas_limit: 30_000_000,
    }
}

/// A minimal finalized block, enough to convert and sign.
fn built_block(bundle: EthrexBlobsBundle) -> BuiltBlock {
    let mut block = ethrex_common::types::Block::default();
    block.header.gas_limit = 30_000_000;
    block.header.gas_used = 21_000;
    block.header.base_fee_per_gas = Some(7);
    block.body.withdrawals = Some(Vec::new());
    BuiltBlock {
        block,
        blobs_bundle: bundle,
        requests: Vec::new(),
        account_updates: Vec::new(),
        block_access_list: None,
        value: U256::from(1_234_567_u64),
    }
}

/// The pre-Gloas shape, which is what a default `ChainInfo` selects.
fn fulu(bid: Bid) -> SignedBidSubmission {
    match bid {
        Bid::Fulu(bid) => bid,
        Bid::Gloas(_) => panic!("a mainnet spec is not on Gloas"),
    }
}

// --- blobs conversion ---

#[test]
fn a_blobs_bundle_converts_to_the_wire_type() {
    let bundle = blob_bundle(1);

    let wire = wire_blobs(&bundle).expect("a cell-proof bundle must convert");

    assert_eq!(wire.blobs.len(), 1);
    assert_eq!(wire.commitments.len(), 1);
    assert_eq!(wire.proofs.len(), CELLS_PER_EXT_BLOB, "128 cell proofs per blob");
    assert_eq!(wire.commitments[0].0, bundle.commitments[0]);
    assert_eq!(wire.blobs[0].as_slice(), bundle.blobs[0].as_slice());
}

#[test]
fn a_bundle_without_cell_proofs_is_refused() {
    let mut bundle = blob_bundle(1);
    // A pre-EIP-7594 bundle: one proof per blob. `version` still reads 0 either
    // way, so only the count distinguishes them.
    bundle.proofs.truncate(1);

    let err = wire_blobs(&bundle).expect_err("the relay requires cell proofs");

    assert!(err.to_string().contains("cell proofs"), "got: {err}");
}

#[test]
fn a_bundle_with_mismatched_commitments_is_refused() {
    let mut bundle = blob_bundle(1);
    bundle.commitments.clear();

    let err = wire_blobs(&bundle).expect_err("commitments must match the blobs");

    assert!(err.to_string().contains("commitments"), "got: {err}");
}

#[test]
fn an_empty_bundle_converts() {
    let wire = wire_blobs(&EthrexBlobsBundle::default()).expect("the common case");

    assert!(wire.blobs.is_empty());
    assert!(wire.proofs.is_empty());
}

// --- the submission ---

#[test]
fn the_bid_trace_mirrors_the_payload() {
    let submitter = submitter("http://localhost:1");
    let built = built_block(EthrexBlobsBundle::default());

    let submission = fulu(submitter.sign(&built, &slot_context()).unwrap());

    // `payload.validate()` on the relay rejects any disagreement here.
    let payload = &submission.execution_payload;
    assert_eq!(submission.message.parent_hash, payload.parent_hash);
    assert_eq!(submission.message.block_hash, payload.block_hash);
    assert_eq!(submission.message.gas_limit, payload.gas_limit);
    assert_eq!(submission.message.gas_used, payload.gas_used);
}

#[test]
fn the_bid_trace_carries_the_slot_and_proposer() {
    let submitter = submitter("http://localhost:1");
    let built = built_block(EthrexBlobsBundle::default());
    let slot = slot_context();

    let submission = fulu(submitter.sign(&built, &slot).unwrap());

    assert_eq!(submission.message.slot, 42);
    assert_eq!(submission.message.proposer_pubkey, slot.proposer_pubkey);
    assert_eq!(submission.message.proposer_fee_recipient, PROPOSER);
    assert_eq!(submission.message.value, built.value);
    assert!(!submission.message.value.is_zero(), "the relay rejects a zero-value block");
}

#[test]
fn the_signature_verifies_under_the_builder_domain() {
    let signing = signing();
    let domain = signing.chain_info.builder_domain;
    let submitter = Submitter::new("http://localhost:1", "key".to_string(), signing);
    let built = built_block(EthrexBlobsBundle::default());

    let submission = fulu(submitter.sign(&built, &slot_context()).unwrap());

    // The exact check the relay's decoder tile makes.
    submission.verify_signature(domain).expect("the relay must accept our signature");
}

#[test]
fn the_submission_round_trips_through_ssz() {
    let submitter = submitter("http://localhost:1");
    let built = built_block(EthrexBlobsBundle::default());
    let submission = fulu(submitter.sign(&built, &slot_context()).unwrap());

    let encoded = ssz::Encode::as_ssz_bytes(&submission);
    let decoded = SignedBidSubmission::from_ssz_bytes(&encoded).expect("the wire format");

    assert_eq!(decoded.message.block_hash, submission.message.block_hash);
    assert_eq!(decoded.message.value, submission.message.value);
}

#[test]
fn a_submission_with_blobs_round_trips() {
    with_large_stack(|| {
        let submitter = submitter("http://localhost:1");
        let built = built_block(blob_bundle(1));

        let submission = fulu(submitter.sign(&built, &slot_context()).unwrap());
        let encoded = ssz::Encode::as_ssz_bytes(&submission);
        let decoded = SignedBidSubmission::from_ssz_bytes(&encoded)
            .expect("a bundle decodes only with 128 proofs per blob");

        assert_eq!(decoded.blobs_bundle.blobs.len(), 1);
        assert_eq!(decoded.blobs_bundle.proofs.len(), CELLS_PER_EXT_BLOB);
    });
}

/// 128 KiB blobs overflow the default stack in debug builds.
fn with_large_stack(test: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(test).unwrap().join().unwrap();
}

// --- the HTTP call ---

/// Serves one canned response and records what the builder sent.
async fn stub_relay(
    status: axum::http::StatusCode,
    body: &'static str,
) -> (String, tokio::sync::oneshot::Receiver<axum::http::HeaderMap>) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let seen = Arc::new(std::sync::Mutex::new(Some(tx)));
    let app = axum::Router::new().route(
        &format!("{PATH_BUILDER_API}{PATH_SUBMIT_BLOCK}"),
        axum::routing::post(move |headers: axum::http::HeaderMap| {
            let seen = seen.clone();
            async move {
                if let Some(tx) = seen.lock().unwrap().take() {
                    let _ = tx.send(headers);
                }
                (status, body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), rx)
}

#[tokio::test]
async fn the_request_carries_the_ssz_headers() {
    let (url, headers) = stub_relay(axum::http::StatusCode::OK, "").await;
    let submitter = submitter(&url);
    let submission =
        submitter.sign(&built_block(EthrexBlobsBundle::default()), &slot_context()).unwrap();

    submitter.submit(&submission).await.unwrap();

    let headers = headers.await.unwrap();
    assert_eq!(headers["content-type"], "application/octet-stream", "SSZ, not JSON");
    assert_eq!(headers["x-api-key"], "test-key");
}

#[tokio::test]
async fn a_success_is_reported() {
    let (url, _headers) = stub_relay(axum::http::StatusCode::OK, "").await;
    let submitter = submitter(&url);
    let submission =
        submitter.sign(&built_block(EthrexBlobsBundle::default()), &slot_context()).unwrap();

    submitter.submit(&submission).await.expect("a 200 is success");
}

#[tokio::test]
async fn a_relay_rejection_is_reported_with_its_body() {
    let (url, _headers) =
        stub_relay(axum::http::StatusCode::BAD_REQUEST, "simulation failed: invalid state root")
            .await;
    let submitter = submitter(&url);
    let submission =
        submitter.sign(&built_block(EthrexBlobsBundle::default()), &slot_context()).unwrap();

    let err = submitter.submit(&submission).await.expect_err("a 400 is not success");

    // This text is how an operator learns the builder is producing bad blocks.
    assert!(err.to_string().contains("invalid state root"), "got: {err}");
    assert!(matches!(err, SubmitError::Rejected { status: 400, .. }), "got: {err}");
}

// --- the Gloas shape ---

/// Gloas from genesis, so `fork_at_slot` reports it for every slot.
fn gloas_signing() -> RelaySigningContext {
    let mut spec = helix_types::ChainSpec::mainnet();
    spec.gloas_fork_epoch = Some(helix_types::Epoch::new(0));
    RelaySigningContext::new(BlsKeypair::random(), Arc::new(ChainInfo::new(spec, B256::ZERO, 0)))
}

fn built_with_bal(bal: Vec<u8>) -> BuiltBlock {
    BuiltBlock { block_access_list: Some(bal), ..built_block(EthrexBlobsBundle::default()) }
}

#[test]
fn a_gloas_submission_carries_the_block_access_list() {
    let submitter = Submitter::new("http://localhost:1", "key".to_string(), gloas_signing());
    let bal = vec![9u8; 64];

    let bid = submitter.sign(&built_with_bal(bal.clone()), &slot_context()).unwrap();

    // Decoded the way the relay's own decoder will, rather than by field access.
    let params = helix_common::decoder::SubmissionDecoderParams::plain(ForkName::Gloas);
    let mut buf = Vec::new();
    let (_, _, _, decoded) = helix_common::decoder::SubmissionDecoder::new(&params)
        .decode(&bid.as_ssz_bytes(), &mut buf)
        .expect("the relay must be able to decode what we send");
    assert_eq!(decoded.expect("Gloas carries a list").to_vec(), bal);
}

#[test]
fn a_fulu_submission_keeps_its_shape() {
    let submitter = submitter("http://localhost:1");
    let built = built_block(EthrexBlobsBundle::default());

    let bid = submitter.sign(&built, &slot_context()).unwrap();

    assert!(matches!(bid, Bid::Fulu(_)));
    let params = helix_common::decoder::SubmissionDecoderParams::plain(ForkName::Fulu);
    let mut buf = Vec::new();
    let (_, _, _, decoded) = helix_common::decoder::SubmissionDecoder::new(&params)
        .decode(&bid.as_ssz_bytes(), &mut buf)
        .expect("the Fulu shape must be unchanged");
    assert!(decoded.is_none(), "only Gloas carries one");
}

/// Refused rather than sent with an empty list, which the simulator would
/// reject as a block hash mismatch with nothing to point at.
#[test]
fn a_gloas_submission_without_a_list_is_refused() {
    let submitter = Submitter::new("http://localhost:1", "key".to_string(), gloas_signing());

    let err = submitter
        .sign(&built_block(EthrexBlobsBundle::default()), &slot_context())
        .expect_err("Gloas needs a list");

    assert!(matches!(err, SubmitError::MissingBlockAccessList), "got: {err}");
}

/// The other direction: an Amsterdam block cannot go out in a pre-Gloas
/// submission, because the shape has nowhere to put its list.
#[test]
fn an_amsterdam_block_in_a_fulu_submission_is_refused() {
    let mut spec = helix_types::ChainSpec::mainnet();
    spec.fulu_fork_epoch = Some(helix_types::Epoch::new(0));
    let signing = RelaySigningContext::new(
        BlsKeypair::random(),
        Arc::new(ChainInfo::new(spec, B256::ZERO, 0)),
    );
    let submitter = Submitter::new("http://localhost:1", "key".to_string(), signing);

    let err = submitter
        .sign(&built_with_bal(vec![1u8; 32]), &slot_context())
        .expect_err("the list would be dropped");

    assert!(matches!(err, SubmitError::UnsubmittableBlockAccessList(ForkName::Fulu)), "got: {err}");
}

/// Step 3 derives the header's slot number from the bid trace, so these two
/// must name the same slot or the simulator rebuilds a different block.
#[test]
fn the_bid_trace_slot_matches_the_slot_the_block_was_built_for() {
    let submitter = Submitter::new("http://localhost:1", "key".to_string(), gloas_signing());
    let slot = slot_context();

    let bid = submitter.sign(&built_with_bal(vec![2u8; 16]), &slot).unwrap();

    assert_eq!(bid.message().slot, slot.slot);
}
