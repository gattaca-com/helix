use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use ethrex_common::types::{
    BYTES_PER_BLOB, Blob, BlobsBundle, CELLS_PER_EXT_BLOB, Commitment, Proof,
};
use ethrex_crypto::kzg::{KzgError, verify_cell_kzg_proof_batch};
use rustc_hash::FxHashMap;

use crate::metrics;

const MAX_ENTRIES: usize = 512;
const RETAIN_BLOCKS: u64 = 4;

struct VerifiedBlob {
    blob: Box<[u8]>,
    proofs: Box<[Proof]>,
    last_seen: AtomicU64,
}

#[derive(Default)]
pub struct BlobCache {
    entries: Mutex<FxHashMap<Commitment, Arc<VerifiedBlob>>>,
}

impl BlobCache {
    pub fn verify(&self, bundle: &BlobsBundle, head: u64) -> Result<bool, KzgError> {
        if bundle.blobs.len() != bundle.commitments.len() ||
            bundle.blobs.len() * CELLS_PER_EXT_BLOB != bundle.proofs.len()
        {
            return Ok(false);
        }
        let cell_proofs =
            |ix: usize| &bundle.proofs[ix * CELLS_PER_EXT_BLOB..(ix + 1) * CELLS_PER_EXT_BLOB];
        let blobs = bundle.blobs.iter().zip(&bundle.commitments);
        let mut missed = Vec::with_capacity(bundle.blobs.len());
        for (ix, ((blob, commitment), proofs)) in
            blobs.zip(bundle.proofs.chunks_exact(CELLS_PER_EXT_BLOB)).enumerate()
        {
            if !self.contains(commitment, blob, proofs, head) {
                missed.push(ix);
            }
        }
        metrics::sim_blob_cache(bundle.blobs.len() - missed.len(), missed.len());
        if missed.is_empty() {
            return Ok(true);
        }

        let valid = if missed.len() == bundle.blobs.len() {
            verify_cell_kzg_proof_batch(&bundle.blobs, &bundle.commitments, &bundle.proofs)?
        } else {
            let mut blobs = vec![[0u8; BYTES_PER_BLOB]; missed.len()];
            let mut commitments = Vec::with_capacity(missed.len());
            let mut proofs = Vec::with_capacity(missed.len() * CELLS_PER_EXT_BLOB);
            for (out, &ix) in blobs.iter_mut().zip(&missed) {
                out.copy_from_slice(&bundle.blobs[ix]);
                commitments.push(bundle.commitments[ix]);
                proofs.extend_from_slice(cell_proofs(ix));
            }
            verify_cell_kzg_proof_batch(&blobs, &commitments, &proofs)?
        };

        if valid {
            for ix in missed {
                self.insert(bundle.commitments[ix], &bundle.blobs[ix], cell_proofs(ix), head);
            }
        }
        Ok(valid)
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
