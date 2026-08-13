//! Run with: `cargo bench -p helix-relay --features bench-internals --bench unbundling`

use std::hint::black_box;

use alloy_primitives::{B256, Bytes, keccak256};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use helix_relay::{OrderTxs, find_unbundled_txs};
use helix_tcp_types::merging::order::{BundleOrderRef, MergeOrderRef};
use rustc_hash::FxHashMap;

const BUNDLE_SIZE: usize = 3;

fn idx_hash(i: u64) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[24..].copy_from_slice(&i.to_be_bytes());
    B256::from(bytes)
}

fn make_order(hashes: Vec<B256>) -> OrderTxs {
    let order_ref = MergeOrderRef::Bundle(BundleOrderRef {
        txs: (0..hashes.len() as u16).collect(),
        reverting_txs: vec![],
        dropping_txs: vec![],
        latest_only: false,
    });
    OrderTxs::from_ref(&order_ref, &hashes)
}

/// `included_orders` are fully satisfied (drawn from `final_txs`); the rest
/// reference hashes absent from `final_txs`, the common case per slot.
fn build_scenario(
    total_orders: usize,
    included_orders: usize,
    final_tx_count: usize,
) -> (Vec<B256>, Vec<OrderTxs>) {
    assert!(included_orders * BUNDLE_SIZE <= final_tx_count);
    assert!(included_orders <= total_orders);

    let final_txs: Vec<B256> = (0..final_tx_count as u64).map(idx_hash).collect();

    let mut orders = Vec::with_capacity(total_orders);
    for i in 0..included_orders {
        let start = i * BUNDLE_SIZE;
        orders.push(make_order(final_txs[start..start + BUNDLE_SIZE].to_vec()));
    }

    let mut next = final_tx_count as u64 + 1;
    for _ in included_orders..total_orders {
        let hashes: Vec<B256> = (0..BUNDLE_SIZE as u64).map(|j| idx_hash(next + j)).collect();
        next += BUNDLE_SIZE as u64;
        orders.push(make_order(hashes));
    }

    (final_txs, orders)
}

/// Scaling with orders sent to a builder connection this slot.
fn bench_by_order_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("unbundling_by_order_count");
    const FINAL_TX_COUNT: usize = 300;
    const INCLUDED_ORDERS: usize = 30;

    for total_orders in [100usize, 1_000, 5_000, 20_000] {
        let (final_txs, orders) = build_scenario(total_orders, INCLUDED_ORDERS, FINAL_TX_COUNT);
        group.throughput(Throughput::Elements(total_orders as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(total_orders),
            &(final_txs, orders),
            |b, (final_txs, orders)| {
                let mut bundled = Vec::new();
                let mut covered = Vec::new();
                b.iter(|| {
                    black_box(find_unbundled_txs(final_txs, orders, &mut bundled, &mut covered))
                });
            },
        );
    }
    group.finish();
}

/// Scaling with the merged block's own tx count.
fn bench_by_final_tx_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("unbundling_by_final_tx_count");
    const TOTAL_ORDERS: usize = 2_000;
    const INCLUDED_ORDERS: usize = 30;

    for final_tx_count in [100usize, 300, 600] {
        let (final_txs, orders) = build_scenario(TOTAL_ORDERS, INCLUDED_ORDERS, final_tx_count);
        group.throughput(Throughput::Elements(final_tx_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(final_tx_count),
            &(final_txs, orders),
            |b, (final_txs, orders)| {
                let mut bundled = Vec::new();
                let mut covered = Vec::new();
                b.iter(|| {
                    black_box(find_unbundled_txs(final_txs, orders, &mut bundled, &mut covered))
                });
            },
        );
    }
    group.finish();
}

/// Cost of resolving a single wire order ref into `OrderTxs`.
fn bench_order_construction(c: &mut Criterion) {
    let tx_hashes: Vec<B256> = (0..BUNDLE_SIZE as u64).map(idx_hash).collect();
    let order_ref = MergeOrderRef::Bundle(BundleOrderRef {
        txs: (0..BUNDLE_SIZE as u16).collect(),
        reverting_txs: vec![],
        dropping_txs: vec![],
        latest_only: false,
    });
    c.bench_function("order_txs_from_ref", |b| {
        b.iter(|| black_box(OrderTxs::from_ref(&order_ref, &tx_hashes)));
    });
}

/// Cost of hashing a merged block's own tx list, the step that produces
/// `find_unbundled_txs`'s `final_txs` input in `tile.rs`.
fn bench_final_tx_hashing(c: &mut Criterion) {
    let mut group = c.benchmark_group("final_tx_hashing");
    for (label, tx_size, count) in
        [("small", 110usize, 300usize), ("typical", 250, 300), ("large", 250, 600)]
    {
        let txs: Vec<Vec<u8>> = (0..count).map(|i| vec![(i % 256) as u8; tx_size]).collect();
        group.throughput(Throughput::Elements(count as u64));
        group.bench_function(label, |b| {
            b.iter(|| {
                let hashes: Vec<B256> = txs.iter().map(|tx| keccak256(tx.as_slice())).collect();
                black_box(hashes)
            });
        });
    }
    group.finish();
}

/// Steady-state cost once a slot's tx bytes are cached: same tx set as
/// `final_tx_hashing/typical`, but the cache is warmed before `b.iter`, as it
/// would be after the outgoing forwarding pass or an earlier merged block.
fn bench_final_tx_hashing_cached(c: &mut Criterion) {
    let txs: Vec<Bytes> = (0..300).map(|i| Bytes::from(vec![(i % 256) as u8; 250])).collect();
    let mut cache: FxHashMap<Bytes, B256> = FxHashMap::default();
    for tx in &txs {
        cache.insert(tx.clone(), keccak256(tx.as_ref()));
    }
    c.bench_function("final_tx_hashing_cached", |b| {
        b.iter(|| {
            let hashes: Vec<B256> = txs
                .iter()
                .map(|tx| *cache.entry(tx.clone()).or_insert_with(|| keccak256(tx.as_ref())))
                .collect();
            black_box(hashes)
        });
    });
}

criterion_group!(
    benches,
    bench_by_order_count,
    bench_by_final_tx_count,
    bench_order_construction,
    bench_final_tx_hashing,
    bench_final_tx_hashing_cached
);
criterion_main!(benches);
