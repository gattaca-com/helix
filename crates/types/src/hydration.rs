use std::{hash::Hasher, sync::Arc};

use rustc_hash::{FxHashMap, FxHasher};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};

use crate::{
    bid_submission,
    fields::{ExecutionRequests, KzgCommitment, KzgProof, Transaction},
    BidTrace, Blob, BlobsBundle, BlsPublicKeyBytes, BlsSignature, ExecutionPayload,
};

/// A bid submission where transactions and blobs may be replaced by hashes instead of payload
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[ssz(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum DehydratedBidSubmission {
    Electra(DehydratedBidSubmissionElectra),
}
impl DehydratedBidSubmission {
    pub fn builder_pubkey(&self) -> &BlsPublicKeyBytes {
        match self {
            DehydratedBidSubmission::Electra(dehydrated_bid_submission_electra) => {
                &dehydrated_bid_submission_electra.message.builder_pubkey
            }
        }
    }

    pub fn hydrate(
        self,
        hydration_cache: &mut HydrationCache,
    ) -> Result<(bid_submission::SignedBidSubmission, usize, usize), HydrationError> {
        match self {
            DehydratedBidSubmission::Electra(dehydrated_bid_submission_electra) => {
                dehydrated_bid_submission_electra.hydrate(hydration_cache).map(
                    |(signed_bid_submission, tx_cache_hits, blob_cache_hits)| {
                        (
                            bid_submission::SignedBidSubmission::Electra(signed_bid_submission),
                            tx_cache_hits,
                            blob_cache_hits,
                        )
                    },
                )
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct DehydratedBidSubmissionElectra {
    message: BidTrace,
    execution_payload: ExecutionPayload,
    blobs_bundle: DehydratedBlobs,
    execution_requests: ExecutionRequests,
    signature: BlsSignature,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
struct DehydratedBlobs {
    proofs: Vec<KzgProof>,
    new_items: Vec<BlobItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
struct BlobItem {
    proof: KzgProof,
    commitment: KzgCommitment,
    blob: Blob,
}

impl DehydratedBidSubmissionElectra {
    /// Hydrates the bid submission, adds new orders to the cache. Returns the hydrated bid
    /// submission and the number of tx cache hits and blob cache hits.
    pub fn hydrate(
        mut self,
        hydration_cache: &mut HydrationCache,
    ) -> Result<(bid_submission::SignedBidSubmissionElectra, usize, usize), HydrationError> {
        let order_cache = hydration_cache.caches.entry(self.message.builder_pubkey).or_default();

        // avoid short-circuiting the loop to maximize cache population
        let mut last_err = Ok(());

        // hydrate transactions

        let mut tx_cache_hits = 0;

        for (index, tx) in self.execution_payload.transactions.iter_mut().enumerate() {
            if tx.len() == std::mem::size_of::<u64>() {
                // hashed transaction, hydrate it
                let bytes = tx.as_ref().try_into().unwrap();
                let hash = u64::from_le_bytes(bytes);
                let Some(cached_tx) = order_cache.transactions.get(&hash) else {
                    last_err = Err(HydrationError::UnknownTxHash { hash, index });
                    continue;
                };

                tx_cache_hits += 1;
                *tx = cached_tx.clone();
            } else {
                /// Max size of the rlp encoded tx sig (v 1 byte, s,r 32 bytes each with a leading
                /// rlp length prefix)
                const TX_KEY_SIZE: usize = 67;

                if tx.len() < TX_KEY_SIZE {
                    last_err = Err(HydrationError::InvalidTxLength { length: tx.len(), index });
                    continue;
                }

                let mut hasher = FxHasher::default();
                let last_slice = &tx[tx.len() - TX_KEY_SIZE..];
                hasher.write(last_slice);
                let hash = hasher.finish();
                order_cache.transactions.insert(hash, tx.clone());
            };
        }

        // hydrate blobs

        let mut blob_cache_hits: usize = 0;
        let new_blobs = self.blobs_bundle.new_items.len();
        for blob in self.blobs_bundle.new_items {
            order_cache.blobs.insert(blob.proof, blob);
        }

        last_err?;

        let mut sidecar = BlobsBundle::with_capacity(self.blobs_bundle.proofs.len());
        for (index, proof) in self.blobs_bundle.proofs.into_iter().enumerate() {
            let Some(item) = order_cache.blobs.get(&proof) else {
                return Err(HydrationError::UnknownBlobHash { proof, index });
            };

            sidecar.commitments.push(item.commitment);
            sidecar.proofs.push(proof);
            sidecar.blobs.push(item.blob.clone());
            blob_cache_hits += 1;
        }

        blob_cache_hits = blob_cache_hits.saturating_sub(new_blobs);

        Ok((
            bid_submission::SignedBidSubmissionElectra {
                message: self.message,
                execution_payload: Arc::new(self.execution_payload),
                blobs_bundle: Arc::new(sidecar),
                execution_requests: Arc::new(self.execution_requests),
                signature: self.signature,
            },
            tx_cache_hits,
            blob_cache_hits,
        ))
    }
}

struct Cache {
    // hash -> transaction bytes
    transactions: FxHashMap<u64, Transaction>,
    // hash -> blob item
    blobs: FxHashMap<KzgProof, BlobItem>,
}

impl Cache {
    pub fn new() -> Self {
        Self {
            transactions: FxHashMap::with_capacity_and_hasher(10_000, Default::default()),
            blobs: FxHashMap::with_capacity_and_hasher(1_000, Default::default()),
        }
    }

    pub fn clear(&mut self) {
        self.transactions.clear();
        self.blobs.clear();
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

/// One cache per builder pubkey
pub struct HydrationCache {
    caches: FxHashMap<BlsPublicKeyBytes, Cache>,
}

impl HydrationCache {
    pub fn new() -> Self {
        Self { caches: FxHashMap::with_capacity_and_hasher(200, Default::default()) }
    }

    pub fn clear(&mut self) {
        for c in self.caches.values_mut() {
            c.clear();
        }
    }
}

impl Default for HydrationCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HydrationError {
    #[error("unkown tx hash {hash}, index {index}")]
    UnknownTxHash { hash: u64, index: usize },

    #[error("invalid tx length {length}, index {index}")]
    InvalidTxLength { length: usize, index: usize },

    #[error("unknown blob hash {proof}, index {index}")]
    UnknownBlobHash { proof: KzgProof, index: usize },
}
