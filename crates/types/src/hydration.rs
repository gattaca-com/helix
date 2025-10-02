use std::{hash::Hasher, sync::Arc};

use alloy_eips::eip7691::MAX_BLOBS_PER_BLOCK_ELECTRA;
use alloy_primitives::B256;
use rustc_hash::{FxHashMap, FxHasher};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use tree_hash::TreeHash;

use crate::{
    bid_submission,
    fields::{ExecutionRequests, KzgCommitment, KzgProof, Transaction},
    BidTrace, Blob, BlobsBundle, BlsPublicKeyBytes, BlsSignatureBytes, ExecutionPayload,
};

/// A bid submission where transactions and blobs may be replaced by hashes instead of payload
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[ssz(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum DehydratedBidSubmission {
    Electra(DehydratedBidSubmissionElectra),
    Fulu(DehydratedBidSubmissionFulu),
}
impl DehydratedBidSubmission {
    pub fn slot(&self) -> u64 {
        match self {
            DehydratedBidSubmission::Electra(s) => s.message.slot,
            DehydratedBidSubmission::Fulu(s) => s.message.slot,
        }
    }

    pub fn block_hash(&self) -> &B256 {
        match self {
            DehydratedBidSubmission::Electra(s) => &s.message.block_hash,
            DehydratedBidSubmission::Fulu(s) => &s.message.block_hash,
        }
    }

    pub fn builder_pubkey(&self) -> &BlsPublicKeyBytes {
        match self {
            DehydratedBidSubmission::Electra(s) => &s.message.builder_pubkey,
            DehydratedBidSubmission::Fulu(s) => &s.message.builder_pubkey,
        }
    }

    pub fn withdrawal_root(&self) -> B256 {
        match self {
            DehydratedBidSubmission::Electra(s) => s.execution_payload.withdrawals.tree_hash_root(),
            DehydratedBidSubmission::Fulu(s) => s.execution_payload.withdrawals.tree_hash_root(),
        }
    }

    pub fn hydrate(
        self,
        hydration_cache: &mut HydrationCache,
        max_blobs_per_block: usize,
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
            DehydratedBidSubmission::Fulu(dehydrated_bid_submission_fulu) => {
                dehydrated_bid_submission_fulu.hydrate(hydration_cache, max_blobs_per_block).map(
                    |(signed_bid_submission, tx_cache_hits, blob_cache_hits)| {
                        (
                            bid_submission::SignedBidSubmission::Fulu(signed_bid_submission),
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
    execution_requests: Arc<ExecutionRequests>,
    signature: BlsSignatureBytes,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct DehydratedBidSubmissionFulu {
    message: BidTrace,
    execution_payload: ExecutionPayload,
    blobs_bundle: DehydratedBlobs,
    execution_requests: Arc<ExecutionRequests>,
    signature: BlsSignatureBytes,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
struct DehydratedBlobs {
    commitments: Vec<KzgCommitment>,
    new_items: Vec<BlobItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
struct BlobItem {
    proof: Vec<KzgProof>,
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
        for blob_item in self.blobs_bundle.new_items {
            order_cache.blobs.insert(blob_item.commitment, (blob_item.proof, blob_item.blob));
        }

        if self.blobs_bundle.commitments.len() > MAX_BLOBS_PER_BLOCK_ELECTRA as usize {
            last_err = Err(HydrationError::TooManyBlobs {
                blobs: self.blobs_bundle.commitments.len(),
                max: MAX_BLOBS_PER_BLOCK_ELECTRA as usize,
            });
        }

        last_err?;

        let mut sidecar = BlobsBundle::with_capacity(self.blobs_bundle.commitments.len());
        for (index, commitment) in self.blobs_bundle.commitments.into_iter().enumerate() {
            let Some((proofs, blob)) = order_cache.blobs.get(&commitment) else {
                return Err(HydrationError::UnknownBlobHash { commitment, index });
            };

            // safe because we checked the length above
            sidecar.commitments.push(commitment).unwrap();
            for proof in proofs {
                sidecar.proofs.push(*proof);
            }
            sidecar.blobs.push(blob.clone());
            blob_cache_hits += 1;
        }

        blob_cache_hits = blob_cache_hits.saturating_sub(new_blobs);

        Ok((
            bid_submission::SignedBidSubmissionElectra {
                message: self.message,
                execution_payload: Arc::new(self.execution_payload),
                blobs_bundle: Arc::new(sidecar),
                execution_requests: self.execution_requests,
                signature: self.signature,
            },
            tx_cache_hits,
            blob_cache_hits,
        ))
    }
}

impl DehydratedBidSubmissionFulu {
    /// Hydrates the bid submission, adds new orders to the cache. Returns the hydrated bid
    /// submission and the number of tx cache hits and blob cache hits.
    pub fn hydrate(
        mut self,
        hydration_cache: &mut HydrationCache,
        max_blobs_per_block: usize,
    ) -> Result<(bid_submission::SignedBidSubmissionFulu, usize, usize), HydrationError> {
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
        for blob_item in self.blobs_bundle.new_items {
            order_cache.blobs.insert(blob_item.commitment, (blob_item.proof, blob_item.blob));
        }

        if self.blobs_bundle.commitments.len() > max_blobs_per_block {
            last_err = Err(HydrationError::TooManyBlobs {
                blobs: self.blobs_bundle.commitments.len(),
                max: max_blobs_per_block,
            });
        }

        last_err?;

        let mut sidecar = BlobsBundle::with_capacity(self.blobs_bundle.commitments.len());
        for (index, commitment) in self.blobs_bundle.commitments.into_iter().enumerate() {
            let Some((proofs, blob)) = order_cache.blobs.get(&commitment) else {
                return Err(HydrationError::UnknownBlobHash { commitment, index });
            };

            // safe because we checked the length above
            sidecar.commitments.push(commitment).unwrap();
            for proof in proofs {
                sidecar.proofs.push(*proof);
            }
            sidecar.blobs.push(blob.clone());
            blob_cache_hits += 1;
        }

        blob_cache_hits = blob_cache_hits.saturating_sub(new_blobs);

        Ok((
            bid_submission::SignedBidSubmissionFulu {
                message: self.message,
                execution_payload: Arc::new(self.execution_payload),
                blobs_bundle: Arc::new(sidecar),
                execution_requests: self.execution_requests,
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
    // commitment -> proofs / blob
    blobs: FxHashMap<KzgCommitment, (Vec<KzgProof>, Blob)>,
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
    #[error("unkown tx: hash {hash}, index {index}")]
    UnknownTxHash { hash: u64, index: usize },

    #[error("invalid tx bytes: length {length}, index {index}")]
    InvalidTxLength { length: usize, index: usize },

    #[error("unknown blob: commitment {commitment}, index {index}")]
    UnknownBlobHash { commitment: KzgCommitment, index: usize },

    #[error("too many blobs: blobs {blobs}, max {max}")]
    TooManyBlobs { blobs: usize, max: usize },
}
