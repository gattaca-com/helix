use std::{hash::Hasher, sync::Arc};

use lh_types::VariableList;
use rustc_hash::{FxHashMap, FxHasher};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};

use crate::{
    bid_submission,
    blobs::{KzgCommitment, KzgProof},
    BidTrace, Blob, BlobsBundle, BlsSignature, ExecutionPayloadElectra, ExecutionRequests,
    Transaction,
};

/// A bid submission where transactions and blobs may be replaced by hashes instead of payload
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[ssz(enum_behaviour = "transparent")]
#[serde(untagged)]
pub enum DehydratedBidSubmission {
    Electra(DehydratedBidSubmissionElectra),
}
impl DehydratedBidSubmission {
    pub fn hydrate(
        self,
        order_cache: &mut HydrationCache,
    ) -> Result<(bid_submission::SignedBidSubmission, usize, usize), HydrationError> {
        match self {
            DehydratedBidSubmission::Electra(dehydrated_bid_submission_electra) => {
                dehydrated_bid_submission_electra.hydrate(order_cache).map(
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
    execution_payload: ExecutionPayloadElectra,
    blobs_bundle: DehydratedBlobs,
    execution_requests: ExecutionRequests,
    signature: BlsSignature,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
struct DehydratedBlobs {
    blobs: Vec<KzgProof>,
    new_items: Vec<BlobItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct BlobItem {
    proof: KzgProof,
    commitment: KzgCommitment,
    blob: Blob,
}

impl DehydratedBidSubmissionElectra {
    /// Hydrates the bid submission, adds new orders to the cache. Returns the hydrated bid
    /// submission and the number of tx cache hits and blob cache hits.
    pub fn hydrate(
        mut self,
        order_cache: &mut HydrationCache,
    ) -> Result<(bid_submission::SignedBidSubmissionElectra, usize, usize), HydrationError> {
        // hydrate transactions

        let mut tx_cache_hits = 0;
        let mut transactions = Vec::with_capacity(self.execution_payload.transactions.len());
        for (index, tx) in self.execution_payload.transactions.into_iter().enumerate() {
            let tx = if tx.len() == std::mem::size_of::<u64>() {
                // hashed transaction, hydrate it
                let bytes = tx.as_ref().try_into().unwrap();
                let hash = u64::from_le_bytes(bytes);
                let Some(tx) = order_cache.transactions.get(&hash) else {
                    return Err(HydrationError::UnknownTxHash { hash, index });
                };

                tx_cache_hits += 1;
                tx.clone()
            } else {
                /// Max size of the rlp encoded tx sig (v 1 byte, s,r 32 bytes each with a leading
                /// rlp length prefix)
                const TX_KEY_SIZE: usize = 67;

                if tx.len() < TX_KEY_SIZE {
                    return Err(HydrationError::InvalidTxLength { length: tx.len(), index });
                }
                let mut hasher = FxHasher::default();
                let last_slice = &tx[tx.len() - TX_KEY_SIZE..];
                hasher.write(last_slice);
                let hash = hasher.finish();
                order_cache.transactions.insert(hash, tx.clone());
                tx
            };

            transactions.push(tx);
        }

        // hydrate blobs

        let mut blob_cache_hits: usize = 0;
        let new_blobs = self.blobs_bundle.new_items.len();
        for blob in self.blobs_bundle.new_items {
            order_cache.blobs.insert(blob.proof, blob);
        }

        let mut sidecar = BlobsBundle::with_capacity(self.blobs_bundle.blobs.len());
        for (index, proof) in self.blobs_bundle.blobs.into_iter().enumerate() {
            let Some(item) = order_cache.blobs.get(&proof) else {
                return Err(HydrationError::UnknownBlobHash { proof, index });
            };

            sidecar.commitments.push(item.commitment);
            sidecar.proofs.push(proof);
            sidecar.blobs.push(item.blob.clone());
            blob_cache_hits += 1;
        }

        blob_cache_hits = blob_cache_hits.saturating_sub(new_blobs);

        self.execution_payload.transactions = VariableList::new(transactions).unwrap();

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

pub struct HydrationCache {
    // TODO: replace lighthouse types to use Bytes so we avoid the extra clone
    // hash -> transaction bytes
    transactions: FxHashMap<u64, Transaction>,
    // hash -> blob item
    blobs: FxHashMap<KzgProof, BlobItem>,
}

impl HydrationCache {
    pub fn new() -> Self {
        Self {
            transactions: FxHashMap::with_capacity_and_hasher(1000, Default::default()),
            blobs: FxHashMap::with_capacity_and_hasher(1000, Default::default()),
        }
    }

    pub fn clear(&mut self) {
        self.transactions.clear();
        self.blobs.clear();
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
