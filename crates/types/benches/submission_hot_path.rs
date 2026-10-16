use std::{hint::black_box, sync::Arc};

use alloy_primitives::B256;
use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use helix_types::{
    BlsKeypair, GloasBidSubmission, GloasBidTrace, SignedBidSubmission, SignedBidSubmissionFulu,
    SignedBidSubmissionGloas, SignedRoot, Slot,
};
use ssz::{Decode, Encode};

const MAX_BLOBS_PER_BLOCK: usize = 21;

type DecodeFn = fn(&[u8]) -> SignedBidSubmission;

fn submissions() -> (B256, Vec<u8>, Vec<u8>) {
    let fulu: SignedBidSubmissionFulu =
        serde_json::from_slice(include_bytes!("../src/testdata/signed-bid-submission-fulu-2.json"))
            .unwrap();
    let keypair = BlsKeypair::random();
    let domain = B256::repeat_byte(1);
    let mut trace = fulu.message.clone();
    trace.builder_pubkey = keypair.pk.serialize().into();
    let message = GloasBidTrace { trace: trace.clone(), parent_block_root: B256::ZERO };
    let gloas = SignedBidSubmissionGloas {
        execution_payload: Arc::new(
            fulu.execution_payload
                .to_lighthouse_gloas_payload(Slot::new(trace.slot), &Default::default())
                .unwrap(),
        ),
        blobs_bundle: fulu.blobs_bundle.clone(),
        execution_requests: Default::default(),
        signature: keypair.sk.sign(message.signing_root(domain)).serialize().into(),
        message,
    };
    let fulu = SignedBidSubmissionFulu {
        signature: keypair.sk.sign(trace.signing_root(domain)).serialize().into(),
        message: trace,
        ..fulu
    };
    (domain, fulu.as_ssz_bytes(), gloas.as_ssz_bytes())
}

fn decode_fulu(bytes: &[u8]) -> SignedBidSubmission {
    SignedBidSubmission::Fulu(SignedBidSubmissionFulu::from_ssz_bytes(bytes).unwrap())
}

fn decode_gloas(bytes: &[u8]) -> SignedBidSubmission {
    SignedBidSubmission::Gloas(
        GloasBidSubmission::from_ssz_bytes(Bytes::copy_from_slice(bytes)).unwrap(),
    )
}

fn hot_path(c: &mut Criterion) {
    let (domain, fulu_bytes, gloas_bytes) = submissions();
    let forks: [(&str, &[u8], DecodeFn); 2] =
        [("fulu", &fulu_bytes, decode_fulu), ("gloas", &gloas_bytes, decode_gloas)];

    let mut group = c.benchmark_group("decode");
    for (fork, bytes, decode) in forks {
        group.bench_function(fork, |b| b.iter(|| decode(black_box(bytes))));
    }
    group.finish();

    let decoded = forks.map(|(fork, bytes, decode)| {
        let submission = decode(bytes);
        submission.verify_signature(domain).unwrap();
        submission.validate_lengths(MAX_BLOBS_PER_BLOCK).unwrap();
        submission.validate().unwrap();
        (fork, submission)
    });

    let mut group = c.benchmark_group("verify_signature");
    for (fork, submission) in &decoded {
        group.bench_function(*fork, |b| b.iter(|| submission.verify_signature(black_box(domain))));
    }
    group.finish();

    let mut group = c.benchmark_group("validate_lengths");
    for (fork, submission) in &decoded {
        group.bench_function(*fork, |b| {
            b.iter(|| submission.validate_lengths(black_box(MAX_BLOBS_PER_BLOCK)))
        });
    }
    group.finish();

    let mut group = c.benchmark_group("validate");
    for (fork, submission) in &decoded {
        group.bench_function(*fork, |b| b.iter(|| black_box(submission).validate()));
    }
    group.finish();

    let mut group = c.benchmark_group("withdrawals_root");
    for (fork, submission) in &decoded {
        group.bench_function(*fork, |b| b.iter(|| black_box(submission).withdrawals_root()));
    }
    group.finish();
}

criterion_group!(benches, hot_path);
criterion_main!(benches);
