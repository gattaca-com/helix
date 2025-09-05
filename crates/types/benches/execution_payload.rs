use std::hint::black_box;

use alloy_rpc_types::engine::ExecutionPayloadV3 as AlloyExecutionPayload;
use criterion::{criterion_group, criterion_main, Criterion};
use helix_types::{ExecutionPayload, SignedBidSubmission};
use lh_types::MainnetEthSpec;
use serde_json;
use ssz::{Decode, Encode};

type LhExecutionPayload = lh_types::ExecutionPayloadElectra<MainnetEthSpec>;

fn benchmark_real_data(c: &mut Criterion) {
    let mut group = c.benchmark_group("execution_payload");

    let data_json = include_bytes!("../src/testdata/signed-bid-submission-electra.json");
    let submission: SignedBidSubmission = serde_json::from_slice(data_json).unwrap();
    let payload = match submission {
        SignedBidSubmission::Electra(ref submission) => &submission.execution_payload,
    };
    let lh_payload = payload.to_lighthouse_electra_paylaod().unwrap();
    let alloy_payload = AlloyExecutionPayload::from_ssz_bytes(&payload.as_ssz_bytes()).unwrap();

    group.bench_function("custom_serde_serialize", |b| {
        b.iter(|| {
            let serialized = serde_json::to_vec(payload).unwrap();
            black_box(serialized);
        });
    });

    group.bench_function("lighthouse_serde_serialize", |b| {
        b.iter(|| {
            let serialized = serde_json::to_vec(&lh_payload).unwrap();
            black_box(serialized);
        });
    });

    group.bench_function("alloy_serde_serialize", |b| {
        b.iter(|| {
            let serialized = serde_json::to_vec(&lh_payload).unwrap();
            black_box(serialized);
        });
    });

    let data_json = serde_json::to_vec(payload).unwrap();
    group.bench_function("custom_serde_deserialize", |b| {
        b.iter(|| {
            let deserialized: ExecutionPayload =
                serde_json::from_slice(data_json.as_slice()).unwrap();
            black_box(deserialized);
        });
    });

    let lh_payload_json = serde_json::to_vec(&lh_payload).unwrap();
    group.bench_function("lighthouse_serde_deserialize", |b| {
        b.iter(|| {
            let deserialized: LhExecutionPayload =
                serde_json::from_slice(&lh_payload_json).unwrap();
            black_box(deserialized);
        });
    });

    let alloy_payload_json = serde_json::to_vec(&alloy_payload).unwrap();
    group.bench_function("alloy_serde_deserialize", |b| {
        b.iter(|| {
            let deserialized: AlloyExecutionPayload =
                serde_json::from_slice(&alloy_payload_json).unwrap();
            black_box(deserialized);
        });
    });

    // ssz

    group.bench_function("custom_ssz_encode", |b| {
        b.iter(|| {
            let encoded = payload.as_ssz_bytes();
            black_box(encoded);
        });
    });

    group.bench_function("lighthouse_ssz_encode", |b| {
        b.iter(|| {
            let encoded = lh_payload.as_ssz_bytes();
            black_box(encoded);
        });
    });

    group.bench_function("alloy_ssz_encode", |b| {
        b.iter(|| {
            let encoded = alloy_payload.as_ssz_bytes();
            black_box(encoded);
        });
    });

    let payload_ssz_bytes = payload.as_ssz_bytes();
    group.bench_function("custom_ssz_decode", |b| {
        b.iter(|| {
            let decoded = ExecutionPayload::from_ssz_bytes(&payload_ssz_bytes).unwrap();
            black_box(decoded);
        });
    });

    group.bench_function("lighthouse_ssz_decode", |b| {
        b.iter(|| {
            let decoded = LhExecutionPayload::from_ssz_bytes(&payload_ssz_bytes).unwrap();
            black_box(decoded);
        });
    });

    group.bench_function("alloy_ssz_decode", |b| {
        b.iter(|| {
            let decoded = AlloyExecutionPayload::from_ssz_bytes(&payload_ssz_bytes).unwrap();
            black_box(decoded);
        });
    });

    group.finish();
}

criterion_group!(benches, benchmark_real_data);
criterion_main!(benches);
