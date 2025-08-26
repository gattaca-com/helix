use std::hint::black_box;

use alloy_rpc_types::engine::BlobsBundleV1;
use criterion::{criterion_group, criterion_main, Criterion};
use helix_types::{BlobsBundle, SignedBidSubmission};
use lh_types::MainnetEthSpec;
use ssz::{Decode, Encode};

fn benchmark_blob_bundle_conversion(c: &mut Criterion) {
    let mut group = c.benchmark_group("blob_bundle_conversion");

    let data_json = include_bytes!("../src/testdata/signed-bid-submission-electra.json");
    let submission: SignedBidSubmission = serde_json::from_slice(data_json).unwrap();
    let blobs_bundle = submission.blobs_bundle().as_ssz_bytes();

    group.bench_function("lighthouse", |b| {
        b.iter(|| {
            let bundle =
                lh_eth2::types::BlobsBundle::<MainnetEthSpec>::from_ssz_bytes(&blobs_bundle)
                    .unwrap();
            black_box(bundle);
        });
    });

    group.bench_function("alloy", |b| {
        b.iter(|| {
            let bundle = BlobsBundleV1::from_ssz_bytes(&blobs_bundle).unwrap();
            black_box(bundle);
        });
    });

    group.bench_function("custom", |b| {
        b.iter(|| {
            let bundle = BlobsBundle::from_ssz_bytes(&blobs_bundle).unwrap();
            black_box(bundle);
        });
    });

    group.finish();
}

criterion_group!(benches, benchmark_blob_bundle_conversion);
criterion_main!(benches);
