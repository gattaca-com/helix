use std::hint::black_box;

use alloy_primitives::B256;
use blst::{BLST_ERROR, min_pk};
use criterion::{Criterion, criterion_group, criterion_main};
use helix_types::{
    BlsPublicKey, BlsSignature, get_fixed_secret,
    gloas::{BlsBatch, bls_verify},
};

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

fn bls(c: &mut Criterion) {
    let root = B256::repeat_byte(1);
    let sk = get_fixed_secret(0);
    let (pk, sig) = (sk.public_key().serialize(), sk.sign(root).serialize());
    c.bench_function("lighthouse", |b| {
        b.iter(|| {
            let pk = BlsPublicKey::deserialize(black_box(&pk)).unwrap();
            assert!(BlsSignature::deserialize(black_box(&sig)).unwrap().verify(&pk, root));
        })
    });
    c.bench_function("blst", |b| {
        b.iter(|| {
            let pk = min_pk::PublicKey::key_validate(black_box(&pk)).unwrap();
            let sig = min_pk::Signature::from_bytes(black_box(&sig)).unwrap();
            let verdict = sig.verify(true, root.as_slice(), DST, &[], &pk, false);
            assert_eq!(verdict, BLST_ERROR::BLST_SUCCESS);
        })
    });
    c.bench_function("silver", |b| {
        b.iter(|| bls_verify(black_box(&pk), black_box(&sig), root).unwrap())
    });
}

/// Verification jobs with distinct keys and precomputed signing roots.
fn batch(c: &mut Criterion) {
    let mut batch = BlsBatch::default();
    let mut job = |items: &[([u8; 48], [u8; 96], B256)], valid: usize| {
        batch.clear();
        for (pk, sig, root) in black_box(items) {
            batch.push(pk, sig, *root);
        }
        batch.verify();
        assert_eq!(batch.verdicts().iter().filter(|v| **v).count(), valid);
    };
    for n in [1, 2, 8, 32, 128] {
        let item = |i: usize, claimed: usize| {
            let sk = min_pk::SecretKey::key_gen(&[i as u8; 32], &[]).unwrap();
            let sig = sk.sign(&[i as u8; 32], DST, &[]).compress();
            (sk.sk_to_pk().compress(), sig, B256::repeat_byte(claimed as u8))
        };
        let valid: Vec<_> = (0..n).map(|i| item(i, i)).collect();
        let mut one_bad = valid.clone();
        one_bad[0] = item(0, n);
        let all_bad: Vec<_> = (0..n).map(|i| item(i, i + n)).collect();
        let mut malformed = valid.clone();
        malformed.iter_mut().for_each(|(_, sig, _)| sig[0] &= 0x7f);

        let mut group = c.benchmark_group(format!("job_{n}"));
        group.bench_function("lighthouse_seq", |b| {
            b.iter(|| {
                for (pk, sig, root) in black_box(&valid) {
                    let pk = BlsPublicKey::deserialize(pk).unwrap();
                    assert!(BlsSignature::deserialize(sig).unwrap().verify(&pk, *root));
                }
            })
        });
        group.bench_function("silver_seq", |b| {
            b.iter(|| {
                for (pk, sig, root) in black_box(&valid) {
                    bls_verify(pk, sig, *root).unwrap();
                }
            })
        });
        group.bench_function("batch_valid", |b| b.iter(|| job(&valid, n)));
        group.bench_function("batch_one_bad", |b| b.iter(|| job(&one_bad, n - 1)));
        group.bench_function("batch_all_bad", |b| b.iter(|| job(&all_bad, 0)));
        group.bench_function("batch_malformed", |b| b.iter(|| job(&malformed, 0)));
        group.finish();
    }
}

criterion_group!(benches, bls, batch);
criterion_main!(benches);
