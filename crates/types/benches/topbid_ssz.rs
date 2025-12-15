use alloy_primitives::{Address, B256, U256};
use criterion::{criterion_group, criterion_main, Criterion};
use helix_types::BlsPublicKeyBytes;
use ssz_derive::{Decode, Encode};
use std::hint::black_box;
use ssz::Encode;

#[derive(Clone, Debug, Encode, Decode)]
pub struct TopBidUpdate {
    pub timestamp: u64,
    pub slot: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub parent_hash: B256,
    pub builder_pubkey: BlsPublicKeyBytes,
    pub fee_recipient: Address,
    pub value: U256,
}

impl TopBidUpdate {
    const SSZ_SIZE: usize = 188;

    pub fn as_ssz_bytes_fast(&self) -> Vec<u8> {
        let mut vec = Vec::with_capacity(Self::SSZ_SIZE);
        self.ssz_append(&mut vec);
        vec
    }
}

fn bench_ssz_encoding(c: &mut Criterion) {
    let tb = TopBidUpdate {
        timestamp: Default::default(),
        slot: Default::default(),
        block_number: Default::default(),
        block_hash: Default::default(),
        parent_hash: Default::default(),
        builder_pubkey: Default::default(),
        fee_recipient: Default::default(),
        value: Default::default(),
    };

    c.bench_function("ssz encode TopBidUpdate", |b| {
        b.iter(|| {
            black_box(tb.as_ssz_bytes_fast());
        })
    });
}

criterion_group!(benches, bench_ssz_encoding);
criterion_main!(benches);
