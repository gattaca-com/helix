use std::hint::black_box;

use alloy_rpc_types::engine::ExecutionPayloadV3 as AlloyExecutionPayload;
use criterion::{criterion_group, criterion_main, Criterion};
use helix_types::{ExecutionPayload, SignedBidSubmission};
use lh_types::MainnetEthSpec;
use serde_json;
use ssz::{Decode, Encode};
use tree_hash::TreeHash;

type LhExecutionPayload = lh_types::ExecutionPayloadElectra<MainnetEthSpec>;

fn benchmark_serde(c: &mut Criterion) {
    let mut group = c.benchmark_group("execution_payload_serde");

    let data_json = include_bytes!("../src/testdata/signed-bid-submission-electra-3.json");
    let submission: SignedBidSubmission = serde_json::from_slice(data_json).unwrap();
    let payload = match submission {
        SignedBidSubmission::Electra(ref submission) => &submission.execution_payload,
    };
    let lh_payload = payload.to_lighthouse_electra_paylaod().unwrap();
    let alloy_payload = AlloyExecutionPayload::from_ssz_bytes(&payload.as_ssz_bytes()).unwrap();

    group.bench_function("custom_serialize", |b| {
        b.iter(|| {
            let serialized = serde_json::to_vec(payload).unwrap();
            black_box(serialized);
        });
    });

    group.bench_function("lighthouse_serialize", |b| {
        b.iter(|| {
            let serialized = serde_json::to_vec(&lh_payload).unwrap();
            black_box(serialized);
        });
    });

    group.bench_function("alloy_serialize", |b| {
        b.iter(|| {
            let serialized = serde_json::to_vec(&alloy_payload).unwrap();
            black_box(serialized);
        });
    });

    let data_json = serde_json::to_vec(payload).unwrap();
    group.bench_function("custom_deserialize", |b| {
        b.iter(|| {
            let deserialized: ExecutionPayload =
                serde_json::from_slice(data_json.as_slice()).unwrap();
            black_box(deserialized);
        });
    });

    let lh_payload_json = serde_json::to_vec(&lh_payload).unwrap();
    group.bench_function("lighthouse_deserialize", |b| {
        b.iter(|| {
            let deserialized: LhExecutionPayload =
                serde_json::from_slice(&lh_payload_json).unwrap();
            black_box(deserialized);
        });
    });

    let alloy_payload_json = serde_json::to_vec(&alloy_payload).unwrap();
    group.bench_function("alloy_deserialize", |b| {
        b.iter(|| {
            let deserialized: AlloyExecutionPayload =
                serde_json::from_slice(&alloy_payload_json).unwrap();
            black_box(deserialized);
        });
    });

    group.finish();
}

fn benchmark_ssz(c: &mut Criterion) {
    let mut group = c.benchmark_group("execution_payload_ssz");

    let data_json = include_bytes!("../src/testdata/signed-bid-submission-electra-3.json");
    let submission: SignedBidSubmission = serde_json::from_slice(data_json).unwrap();
    let payload = match submission {
        SignedBidSubmission::Electra(ref submission) => &submission.execution_payload,
    };
    let lh_payload = payload.to_lighthouse_electra_paylaod().unwrap();
    let alloy_payload = AlloyExecutionPayload::from_ssz_bytes(&payload.as_ssz_bytes()).unwrap();

    group.bench_function("custom_encode", |b| {
        b.iter(|| {
            let encoded = payload.as_ssz_bytes();
            black_box(encoded);
        });
    });

    group.bench_function("lighthouse_encode", |b| {
        b.iter(|| {
            let encoded = lh_payload.as_ssz_bytes();
            black_box(encoded);
        });
    });

    group.bench_function("alloy_encode", |b| {
        b.iter(|| {
            let encoded = alloy_payload.as_ssz_bytes();
            black_box(encoded);
        });
    });

    let payload_ssz_bytes = payload.as_ssz_bytes();
    group.bench_function("custom_decode", |b| {
        b.iter(|| {
            let decoded = ExecutionPayload::from_ssz_bytes(&payload_ssz_bytes).unwrap();
            black_box(decoded);
        });
    });

    group.bench_function("lighthouse_decode", |b| {
        b.iter(|| {
            let decoded = LhExecutionPayload::from_ssz_bytes(&payload_ssz_bytes).unwrap();
            black_box(decoded);
        });
    });

    group.bench_function("alloy_decode", |b| {
        b.iter(|| {
            let decoded = AlloyExecutionPayload::from_ssz_bytes(&payload_ssz_bytes).unwrap();
            black_box(decoded);
        });
    });

    group.finish();
}

fn benchmark_transaction_root(c: &mut Criterion) {
    let mut group = c.benchmark_group("transaction_root");

    let data_json = include_bytes!("../src/testdata/signed-bid-submission-electra-3.json");
    let submission: SignedBidSubmission = serde_json::from_slice(data_json).unwrap();
    let payload = match submission {
        SignedBidSubmission::Electra(ref submission) => &submission.execution_payload,
    };
    let lh_payload = payload.to_lighthouse_electra_paylaod().unwrap();

    group.bench_function("custom_transaction_root", |b| {
        b.iter(|| {
            let root = payload.transaction_root();
            black_box(root);
        });
    });

    group.bench_function("lighthouse_transaction_root", |b| {
        b.iter(|| {
            let root = lh_payload.transactions.tree_hash_root();
            black_box(root);
        });
    });

    group.finish();
}

criterion_group!(benches, benchmark_serde, benchmark_ssz, benchmark_transaction_root);
criterion_main!(benches);
