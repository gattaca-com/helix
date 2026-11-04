//! Compares transaction, withdrawal and BuilderBid signing roots through
//! `tree_hash` and `silver_ssz`, plus Lighthouse and blst signing.
//! Transaction lengths are sampled uniformly from 100..=600 bytes.
//! Both BLS cases sign a precomputed root and compress the signature.

use std::hint::black_box;

use alloy_primitives::{B256, FixedBytes};
use blst::min_pk;
use criterion::{Criterion, criterion_group, criterion_main};
use helix_types::{
    BuilderBid, KzgCommitments, SignedBidSubmissionFulu, SigningData, TestRandom, Transaction,
    Transactions, Withdrawal, Withdrawals, get_fixed_secret,
};
use rand::{Rng, RngCore, SeedableRng};
use rand_xorshift::XorShiftRng;
use tree_hash::TreeHash;

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

fn roots(c: &mut Criterion) {
    let mut rng = XorShiftRng::from_seed([1; 16]);
    let fixture: SignedBidSubmissionFulu =
        serde_json::from_slice(include_bytes!("../src/testdata/signed-bid-submission-fulu-2.json"))
            .unwrap();
    let mut payload = fixture.execution_payload.as_ref().clone();

    for n in [100, 300, 1000] {
        let txs = (0..n).map(|_| {
            let mut tx = vec![0; rng.random_range(100..=600)];
            rng.fill_bytes(&mut tx);
            Transaction(tx.into())
        });
        payload.transactions = Transactions::new(txs.collect()).unwrap();
        assert_eq!(payload.transactions.tree_hash_root(), payload.transaction_root());
        let mut group = c.benchmark_group(format!("transactions_root_{n}"));
        group.bench_function("tree_hash", |b| {
            b.iter(|| black_box(&payload.transactions).tree_hash_root())
        });
        group.bench_function("silver", |b| b.iter(|| black_box(&payload).transaction_root()));
        group.finish();
    }

    let withdrawals = (0..16).map(|_| Withdrawal::random_for_test(&mut rng));
    payload.withdrawals = Withdrawals::new(withdrawals.collect()).unwrap();
    assert_eq!(payload.withdrawals.tree_hash_root(), payload.withdrawals_root());
    let mut group = c.benchmark_group("withdrawals_root_16");
    group.bench_function("tree_hash", |b| {
        b.iter(|| black_box(&payload.withdrawals).tree_hash_root())
    });
    group.bench_function("silver", |b| b.iter(|| black_box(&payload).withdrawals_root()));
    group.finish();

    let commitments = (0..6).map(|_| FixedBytes::random()).collect();
    let bid = BuilderBid {
        header: payload.to_header(None, None),
        blob_kzg_commitments: KzgCommitments::new(commitments).unwrap(),
        execution_requests: fixture.execution_requests.clone(),
        value: fixture.message.value,
        pubkey: fixture.message.builder_pubkey,
    };
    let domain = B256::repeat_byte(1);
    let root = bid.signing_root(domain);
    assert_eq!(SigningData { object_root: bid.tree_hash_root(), domain }.tree_hash_root(), root);
    let mut group = c.benchmark_group("builder_bid_signing_root");
    group.bench_function("tree_hash", |b| {
        b.iter(|| {
            let object_root = black_box(&bid).tree_hash_root();
            SigningData { object_root, domain }.tree_hash_root()
        })
    });
    group.bench_function("silver", |b| b.iter(|| black_box(&bid).signing_root(domain)));
    group.finish();

    let lighthouse = get_fixed_secret(0);
    let blst = min_pk::SecretKey::from_bytes(lighthouse.serialize().as_bytes()).unwrap();
    assert_eq!(lighthouse.sign(root).serialize(), blst.sign(root.as_slice(), DST, &[]).compress());
    let mut group = c.benchmark_group("relay_sign");
    group.bench_function("lighthouse", |b| b.iter(|| lighthouse.sign(black_box(root)).serialize()));
    group.bench_function("blst", |b| {
        b.iter(|| blst.sign(black_box(root).as_slice(), DST, &[]).compress())
    });
    group.finish();
}

criterion_group!(benches, roots);
criterion_main!(benches);
