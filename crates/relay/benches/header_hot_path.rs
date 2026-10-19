//! Run with: `cargo bench -p helix-relay --features bench-internals --bench header_hot_path`

use std::{hint::black_box, sync::Arc};

use alloy_primitives::{Address, B256};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use helix_common::{SubmissionTrace, chain_info::ChainInfo, signing::RelaySigningContext};
use helix_relay::{GloasBuilderIdentity, GloasPayload, PayloadEntry, resign_builder_bid};
use helix_types::{
    BlsKeypair, ForkName, GloasBidSubmission, GloasBidTrace, SignedBidSubmissionFulu,
    SignedBidSubmissionGloas, Slot, SubmissionVersion, ssz_json,
};
use ssz::Encode;

fn submissions() -> (SignedBidSubmissionFulu, GloasBidSubmission) {
    let fulu: SignedBidSubmissionFulu = serde_json::from_slice(include_bytes!(
        "../../types/src/testdata/signed-bid-submission-fulu-2.json"
    ))
    .unwrap();
    let gloas = SignedBidSubmissionGloas {
        message: GloasBidTrace { trace: fulu.message.clone(), parent_block_root: B256::ZERO },
        execution_payload: Arc::new(
            fulu.execution_payload
                .to_lighthouse_gloas_payload(Slot::new(fulu.message.slot), &Default::default())
                .unwrap(),
        ),
        blobs_bundle: fulu.blobs_bundle.clone(),
        execution_requests: Default::default(),
        signature: fulu.signature,
    };
    let gloas = GloasBidSubmission::from_ssz_bytes(gloas.as_ssz_bytes().into()).unwrap();
    (fulu, gloas)
}

fn fulu_entry(submission: SignedBidSubmissionFulu, withdrawals_root: B256) -> PayloadEntry {
    PayloadEntry::new_submission(
        submission,
        withdrawals_root,
        None,
        None,
        SubmissionVersion::new(0, None),
        SubmissionTrace::default(),
        None,
    )
}

fn hot_path(c: &mut Criterion) {
    let (fulu, gloas) = submissions();
    let chain_info = ChainInfo::default();
    let signing = RelaySigningContext::new(BlsKeypair::random(), Arc::new(chain_info.clone()));
    let identity = GloasBuilderIdentity::new(BlsKeypair::random());
    let withdrawals_root = fulu.withdrawals_root();
    identity.set_builder_index(Some(2));

    let mut group = c.benchmark_group("store_entry");
    group.bench_function("fulu", |b| {
        b.iter_batched(|| fulu.clone(), |s| fulu_entry(s, withdrawals_root), BatchSize::SmallInput)
    });
    group.bench_function("gloas", |b| {
        b.iter_batched(
            || gloas.clone(),
            |s| GloasPayload::new(s, Address::ZERO),
            BatchSize::SmallInput,
        )
    });
    group.finish();

    let fulu_entry = fulu_entry(fulu, withdrawals_root);
    let gloas_entry = GloasPayload::new(gloas, Address::ZERO);
    let fulu_signed = || {
        let bid = black_box(&fulu_entry).clone().into_builder_bid_slow();
        resign_builder_bid(bid, &signing, ForkName::Fulu)
    };
    let gloas_signed = || identity.sign_bid(&black_box(&gloas_entry).bid, &chain_info).unwrap();

    let mut group = c.benchmark_group("get_header_ssz");
    group.bench_function("fulu", |b| b.iter(|| fulu_signed().data.as_ssz_bytes()));
    group.bench_function("gloas", |b| b.iter(gloas_signed));
    group.finish();

    let mut group = c.benchmark_group("get_header_json");
    group.bench_function("fulu", |b| {
        b.iter(|| serde_json::to_vec(&serde_json::to_value(fulu_signed()).unwrap()).unwrap())
    });
    group.bench_function("gloas", |b| {
        b.iter(|| {
            let data = ssz_json::to_json(&ssz_json::SIGNED_EXECUTION_PAYLOAD_BID, &gloas_signed())
                .unwrap();
            serde_json::to_vec(&serde_json::json!({ "version": ForkName::Gloas, "data": data }))
                .unwrap()
        })
    });
    group.finish();
}

criterion_group!(benches, hot_path);
criterion_main!(benches);
