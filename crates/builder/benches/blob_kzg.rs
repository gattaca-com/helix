use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use ethrex_common::types::BlobsBundle;
use ethrex_crypto::kzg::verify_cell_kzg_proof_batch;
use rand::{RngCore, SeedableRng};
use rand_xorshift::XorShiftRng;
use silver_crypto_simd::kzg::{
    BYTES_PER_BLOB, BYTES_PER_CELL, BYTES_PER_FIELD_ELEMENT, CELLS_PER_EXT_BLOB, CellBatchVerifier,
    CellExtender,
};

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn blob_kzg(c: &mut Criterion) {
    let mut rng = XorShiftRng::seed_from_u64(1);
    let blobs = (0..21)
        .map(|_| {
            let mut blob = [0; BYTES_PER_BLOB];
            rng.fill_bytes(&mut blob);
            blob.iter_mut().step_by(BYTES_PER_FIELD_ELEMENT).for_each(|byte| *byte = 0);
            blob
        })
        .collect();
    let bundle = BlobsBundle::create_from_blobs(&blobs, Some(1)).unwrap();
    let mut extender = CellExtender::new();
    let mut verifier = CellBatchVerifier::new();
    let (mut cell_commitments, mut cell_proofs) = (Vec::new(), Vec::new());
    for n in [1, 6, 21] {
        let (blobs, commitments) = (&bundle.blobs[..n], &bundle.commitments[..n]);
        let proofs = &bundle.proofs[..n * CELLS_PER_EXT_BLOB];
        let indices: Vec<_> = (0..n).flat_map(|_| 0..CELLS_PER_EXT_BLOB as u64).collect();
        let mut cells = vec![[0; BYTES_PER_CELL]; n * CELLS_PER_EXT_BLOB];
        let mut group = c.benchmark_group(format!("verify_{n}"));
        group.bench_function("ckzg", |b| {
            b.iter(|| assert!(verify_cell_kzg_proof_batch(blobs, commitments, proofs).unwrap()))
        });
        group.bench_function("silver", |b| {
            b.iter(|| {
                cell_commitments.clear();
                cell_proofs.clear();
                let blobs =
                    blobs.iter().zip(commitments).zip(proofs.chunks_exact(CELLS_PER_EXT_BLOB));
                for (((blob, commitment), blob_proofs), out) in blobs.zip(cells.as_chunks_mut().0) {
                    assert!(extender.compute(blob, out));
                    cell_commitments.extend(std::iter::repeat_n(*commitment, CELLS_PER_EXT_BLOB));
                    cell_proofs.extend_from_slice(blob_proofs);
                }
                assert!(verifier.verify(&cell_commitments, &indices, &cells, &cell_proofs));
            })
        });
        group.finish();
    }
    c.bench_function("silver_new", |b| {
        b.iter_batched(
            || (),
            |()| (CellExtender::new(), CellBatchVerifier::new()),
            BatchSize::PerIteration,
        )
    });
}

criterion_group!(benches, blob_kzg);
criterion_main!(benches);
