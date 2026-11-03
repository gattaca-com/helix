use std::hint::black_box;

use alloy_primitives::B256;
use blst::{BLST_ERROR, min_pk};
use criterion::{Criterion, criterion_group, criterion_main};
use helix_types::{BlsPublicKey, BlsSignature, get_fixed_secret, gloas::bls_verify};

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

criterion_group!(benches, bls);
criterion_main!(benches);
