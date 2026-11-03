use std::{
    cell::RefCell,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use ethrex_common::types::{Blob, BlobsBundle, CELLS_PER_EXT_BLOB, Commitment, Proof};
use rustc_hash::FxHashMap;
use silver_crypto_simd::kzg::{BYTES_PER_CELL, CellBatchVerifier, CellExtender};

use crate::metrics;

const MAX_ENTRIES: usize = 512;
const RETAIN_BLOCKS: u64 = 4;

struct VerifiedBlob {
    blob: Box<[u8]>,
    proofs: Box<[Proof]>,
    last_seen: AtomicU64,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

#[derive(Default)]
struct State {
    extender: CellExtender,
    verifier: CellBatchVerifier,
    missed: Vec<usize>,
    cells: Vec<[u8; BYTES_PER_CELL]>,
    commitments: Vec<Commitment>,
    indices: Vec<u64>,
    proofs: Vec<Proof>,
}

impl State {
    fn verify(&mut self, cache: &BlobCache, bundle: &BlobsBundle, head: u64) -> bool {
        self.missed.clear();
        self.commitments.clear();
        self.proofs.clear();

        let blobs = bundle.blobs.iter().zip(&bundle.commitments);
        let mut extended = true;
        let mut hits = 0;
        for (ix, ((blob, commitment), proofs)) in
            blobs.zip(bundle.proofs.chunks_exact(CELLS_PER_EXT_BLOB)).enumerate()
        {
            if cache.contains(commitment, blob, proofs, head) {
                hits += 1;
                continue;
            }
            if !extended {
                continue;
            }
            // Both buffers retain equal lengths in whole blobs. `self.proofs.len()`
            // marks the active prefix, so `start` cannot exceed their retained length.
            let start = self.proofs.len();
            if self.cells.len() == start {
                self.cells.resize(start + CELLS_PER_EXT_BLOB, [0; BYTES_PER_CELL]);
                self.indices.extend(0..CELLS_PER_EXT_BLOB as u64);
            }
            let out = self.cells[start..].first_chunk_mut().expect("cells cover this blob");
            if !self.extender.compute(blob, out) {
                extended = false;
                continue;
            }
            self.missed.push(ix);
            self.commitments.extend(std::iter::repeat_n(*commitment, CELLS_PER_EXT_BLOB));
            self.proofs.extend_from_slice(proofs);
        }
        metrics::sim_blob_cache(hits, bundle.blobs.len() - hits);
        if !extended {
            return false;
        }

        let active = self.proofs.len();
        let valid = self.missed.is_empty() ||
            self.verifier.verify(
                &self.commitments,
                &self.indices[..active],
                &self.cells[..active],
                &self.proofs,
            );
        if valid {
            for (&ix, proofs) in
                self.missed.iter().zip(self.proofs.chunks_exact(CELLS_PER_EXT_BLOB))
            {
                cache.insert(bundle.commitments[ix], &bundle.blobs[ix], proofs, head);
            }
        }
        valid
    }
}

#[derive(Default)]
pub struct BlobCache {
    entries: Mutex<FxHashMap<Commitment, Arc<VerifiedBlob>>>,
}

impl BlobCache {
    pub fn verify(&self, bundle: &BlobsBundle, head: u64) -> bool {
        if bundle.blobs.len() != bundle.commitments.len() ||
            bundle.blobs.len() * CELLS_PER_EXT_BLOB != bundle.proofs.len()
        {
            return false;
        }
        STATE.with_borrow_mut(|state| state.verify(self, bundle, head))
    }

    fn contains(&self, commitment: &Commitment, blob: &Blob, proofs: &[Proof], head: u64) -> bool {
        let Some(entry) = self.entries.lock().ok().and_then(|e| e.get(commitment).cloned()) else {
            return false;
        };
        if *entry.proofs != *proofs || *entry.blob != blob[..] {
            return false;
        }
        entry.last_seen.store(head, Ordering::Relaxed);
        true
    }

    fn insert(&self, commitment: Commitment, blob: &Blob, proofs: &[Proof], head: u64) {
        let entry = Arc::new(VerifiedBlob {
            blob: blob.as_slice().into(),
            proofs: proofs.into(),
            last_seen: AtomicU64::new(head),
        });
        let Ok(mut entries) = self.entries.lock() else { return };
        if entries.len() >= MAX_ENTRIES {
            entries.retain(|_, e| e.last_seen.load(Ordering::Relaxed) + RETAIN_BLOCKS >= head);
            if entries.len() >= MAX_ENTRIES {
                return;
            }
        }
        entries.insert(commitment, entry);
    }
}

#[cfg(test)]
mod tests {
    use ethrex_crypto::kzg::verify_cell_kzg_proof_batch;

    use super::*;
    use crate::testing::blob_bundle;

    #[test]
    fn verdict_equals_ckzg() {
        type Corrupt = fn(&mut BlobsBundle);
        let cases: [(&str, Corrupt, bool); 5] = [
            ("valid", |_| {}, true),
            ("flipped proof byte", |b| b.proofs[CELLS_PER_EXT_BLOB + 5][10] ^= 1, false),
            ("swapped commitments", |b| b.commitments.swap(0, 1), false),
            ("proofs at swapped cells", |b| b.proofs.swap(5, 6), false),
            ("element not below modulus", |b| b.blobs[1][32..64].fill(0xff), false),
        ];
        let valid = blob_bundle(2);
        for (name, corrupt, expected) in cases {
            let mut bundle = valid.clone();
            corrupt(&mut bundle);
            assert_eq!(BlobCache::default().verify(&bundle, 1), expected, "{name}");
            assert_eq!(
                verify_cell_kzg_proof_batch(&bundle.blobs, &bundle.commitments, &bundle.proofs)
                    .unwrap_or(false),
                expected,
                "{name}: oracle"
            );
        }
    }

    #[test]
    fn cache_admits_only_verified_blobs() {
        let valid = blob_bundle(2);
        let mut first = valid.clone();
        first.blobs.truncate(1);
        first.commitments.truncate(1);
        first.proofs.truncate(CELLS_PER_EXT_BLOB);
        let mut invalid = valid.clone();
        invalid.proofs[CELLS_PER_EXT_BLOB + 5][10] ^= 1;
        let mut noncanonical = valid.clone();
        noncanonical.blobs[1][32..64].fill(0xff);

        let cache = BlobCache::default();
        assert!(!cache.verify(&noncanonical, 1));
        assert!(cache.verify(&first, 1), "smaller bundle after an extension failure");
        assert!(!cache.verify(&invalid, 1), "cache hit masks an invalid sibling");
        assert!(!cache.verify(&invalid, 1), "rejected blob was cached");
        assert!(cache.verify(&valid, 1), "cached blob plus a new valid blob");
    }
}
