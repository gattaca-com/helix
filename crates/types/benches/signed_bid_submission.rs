use std::hint::black_box;

use alloy_rpc_types::beacon::relay::SignedBidSubmissionV4 as AlloySignedBidSubmission;
use criterion::{criterion_group, criterion_main, Criterion};
use helix_types::SignedBidSubmission;
use ssz::{Decode, Encode};

fn benchmark_signed_bid_submission(c: &mut Criterion) {
    let mut group = c.benchmark_group("signed_bid_submission");

    let data_json = include_bytes!("../src/testdata/signed-bid-submission-electra.json");
    let custom_submission: SignedBidSubmission = serde_json::from_slice(data_json).unwrap();
    let ssz_bytes = custom_submission.as_ssz_bytes();

    let alloy_submission = AlloySignedBidSubmission::from_ssz_bytes(&ssz_bytes).unwrap();
    group.bench_function("alloy ssz encode", |b| {
        b.iter(|| {
            let r = alloy_submission.as_ssz_bytes();
            black_box(r);
        });
    });

    group.bench_function("custom ssz encode", |b| {
        b.iter(|| {
            let r = custom_submission.as_ssz_bytes();
            black_box(r);
        });
    });

    group.bench_function("alloy ssz decode", |b| {
        b.iter(|| {
            let r = AlloySignedBidSubmission::from_ssz_bytes(&ssz_bytes).unwrap();
            black_box(r);
        });
    });

    group.bench_function("custom ssz decode", |b| {
        b.iter(|| {
            let r = SignedBidSubmission::from_ssz_bytes(&ssz_bytes).unwrap();
            black_box(r);
        });
    });

    let json = serde_json::to_vec(&custom_submission).unwrap();
    group.bench_function("custom serde serialize", |b| {
        b.iter(|| {
            let r = serde_json::to_vec(&custom_submission).unwrap();
            black_box(r);
        });
    });

    group.bench_function("custom serde deserialize", |b| {
        b.iter(|| {
            let r: SignedBidSubmission = serde_json::from_slice(&json).unwrap();
            black_box(r);
        });
    });

    let alloy_json = serde_json::to_vec(&alloy_submission).unwrap();
    group.bench_function("alloy serde serialize", |b| {
        b.iter(|| {
            let r = serde_json::to_vec(&alloy_submission).unwrap();
            black_box(r);
        });
    });

    group.bench_function("alloy serde deserialize", |b| {
        b.iter(|| {
            let r: AlloySignedBidSubmission = serde_json::from_slice(&alloy_json).unwrap();
            black_box(r);
        });
    });

    group.finish();
}

criterion_group!(benches, benchmark_signed_bid_submission);
criterion_main!(benches);
