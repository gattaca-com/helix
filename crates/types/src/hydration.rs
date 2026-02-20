use std::{hash::Hasher, sync::Arc};

use alloy_primitives::{Address, B256};
use lh_types::{ForkName, ForkVersionDecode};
use rustc_hash::{FxHashMap, FxHasher};
use serde::{Deserialize, Serialize};
use ssz::{Decode, DecodeError};
use ssz_derive::{Decode, Encode};
use tracing::trace;
use tree_hash::TreeHash;

use crate::{
    BidTrace, Blob, BlobsBundle, BlobsBundleV2, BlsPublicKeyBytes, BlsSignatureBytes,
    ExecutionPayload, SignedBidSubmission,
    bid_adjustment_data::BidAdjustmentData,
    bid_submission,
    fields::{ExecutionRequests, KzgCommitment, KzgProof, Transaction},
};

/// A bid submission where transactions and blobs may be replaced by hashes instead of payload
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DehydratedBidSubmission {
    Fulu(DehydratedBidSubmissionFulu),
}

impl ForkVersionDecode for DehydratedBidSubmission {
    fn from_ssz_bytes_by_fork(bytes: &[u8], fork: ForkName) -> Result<Self, DecodeError> {
        match fork {
            ForkName::Base
            | ForkName::Altair
            | ForkName::Bellatrix
            | ForkName::Capella
            | ForkName::Deneb
            | ForkName::Electra
            | ForkName::Gloas => Err(DecodeError::NoMatchingVariant),
            ForkName::Fulu => DehydratedBidSubmissionFulu::from_ssz_bytes(bytes)
                .map(DehydratedBidSubmission::Fulu),
        }
    }
}

pub struct HydratedData {
    pub submission: bid_submission::SignedBidSubmission,
    pub tx_cache_hits: usize,
    pub blob_cache_hits: usize,
    pub tx_root: Option<B256>,
}

impl DehydratedBidSubmission {
    pub fn slot(&self) -> u64 {
        match self {
            DehydratedBidSubmission::Fulu(s) => s.message.slot,
        }
    }

    pub fn block_hash(&self) -> &B256 {
        match self {
            DehydratedBidSubmission::Fulu(s) => &s.message.block_hash,
        }
    }

    pub fn builder_pubkey(&self) -> &BlsPublicKeyBytes {
        match self {
            DehydratedBidSubmission::Fulu(s) => &s.message.builder_pubkey,
        }
    }

    pub fn fee_recipient(&self) -> Address {
        match self {
            DehydratedBidSubmission::Fulu(s) => s.message.proposer_fee_recipient,
        }
    }

    pub fn withdrawal_root(&self) -> B256 {
        match self {
            DehydratedBidSubmission::Fulu(s) => s.execution_payload.withdrawals.tree_hash_root(),
        }
    }

    pub fn parent_hash(&self) -> &B256 {
        match self {
            DehydratedBidSubmission::Fulu(s) => &s.message.parent_hash,
        }
    }

    pub fn hydrate(
        self,
        hydration_cache: &mut HydrationCache,
        max_blobs_per_block: usize,
    ) -> Result<HydratedData, HydrationError> {
        match self {
            DehydratedBidSubmission::Fulu(s) => s.hydrate(hydration_cache, max_blobs_per_block),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct DehydratedBidSubmissionFulu {
    message: BidTrace,
    execution_payload: ExecutionPayload,
    blobs_bundle: DehydratedBlobsFulu,
    execution_requests: Arc<ExecutionRequests>,
    signature: BlsSignatureBytes,
    tx_root: Option<B256>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct DehydratedBidSubmissionFuluWithAdjustments {
    message: BidTrace,
    execution_payload: ExecutionPayload,
    blobs_bundle: DehydratedBlobsFulu,
    execution_requests: Arc<ExecutionRequests>,
    signature: BlsSignatureBytes,
    tx_root: Option<B256>,
    bid_adjustment_data: BidAdjustmentData,
}

impl DehydratedBidSubmissionFuluWithAdjustments {
    pub fn split(self) -> (DehydratedBidSubmission, BidAdjustmentData) {
        (
            DehydratedBidSubmission::Fulu(DehydratedBidSubmissionFulu {
                message: self.message,
                execution_payload: self.execution_payload,
                blobs_bundle: self.blobs_bundle,
                execution_requests: self.execution_requests,
                signature: self.signature,
                tx_root: self.tx_root,
            }),
            self.bid_adjustment_data,
        )
    }
}

impl ForkVersionDecode for DehydratedBidSubmissionFuluWithAdjustments {
    fn from_ssz_bytes_by_fork(bytes: &[u8], fork: ForkName) -> Result<Self, DecodeError> {
        match fork {
            ForkName::Base
            | ForkName::Altair
            | ForkName::Bellatrix
            | ForkName::Capella
            | ForkName::Deneb
            | ForkName::Gloas
            | ForkName::Electra => Err(DecodeError::NoMatchingVariant),
            ForkName::Fulu => DehydratedBidSubmissionFuluWithAdjustments::from_ssz_bytes(bytes),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
struct DehydratedBlobsFulu {
    commitments: Vec<KzgCommitment>,
    new_items: Vec<BlobItemFulu>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
struct BlobItemFulu {
    proof: Vec<KzgProof>,
    commitment: KzgCommitment,
    blob: Blob,
}

impl DehydratedBidSubmissionFulu {
    /// Hydrates the bid submission, adds new orders to the cache. Returns the hydrated bid
    /// submission and the number of tx cache hits and blob cache hits.
    pub fn hydrate(
        mut self,
        hydration_cache: &mut HydrationCache,
        max_blobs_per_block: usize,
    ) -> Result<HydratedData, HydrationError> {
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
                    last_err = Err(HydrationError::UnknownTxHash { index, hash });
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
                trace!("Inserted tx into cache: index {}, hash {}", index, hash);
            };
        }

        // hydrate blobs

        let mut blob_cache_hits: usize = 0;
        let new_blobs = self.blobs_bundle.new_items.len();
        for blob_item in self.blobs_bundle.new_items {
            order_cache.blobs_fulu.insert(blob_item.commitment, (blob_item.proof, blob_item.blob));
        }

        if self.blobs_bundle.commitments.len() > max_blobs_per_block {
            last_err = Err(HydrationError::TooManyBlobs {
                blobs: self.blobs_bundle.commitments.len(),
                max: max_blobs_per_block,
            });
        }

        last_err?;

        let mut sidecar = BlobsBundleV2::with_capacity(self.blobs_bundle.commitments.len());
        for (index, commitment) in self.blobs_bundle.commitments.into_iter().enumerate() {
            let Some((proofs, blob)) = order_cache.blobs_fulu.get(&commitment) else {
                return Err(HydrationError::UnknownBlobHashFulu { commitment, index });
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

        let submission = SignedBidSubmission {
            message: self.message,
            execution_payload: Arc::new(self.execution_payload),
            blobs_bundle: Arc::new(BlobsBundle::V2(sidecar)),
            execution_requests: self.execution_requests,
            signature: self.signature,
        };

        Ok(HydratedData { submission, tx_cache_hits, blob_cache_hits, tx_root: self.tx_root })
    }
}

struct Cache {
    // hash -> transaction bytes
    transactions: FxHashMap<u64, Transaction>,
    // commitment -> proofs / blob
    blobs_fulu: FxHashMap<KzgCommitment, (Vec<KzgProof>, Blob)>,
}

impl Cache {
    pub fn new() -> Self {
        Self {
            transactions: FxHashMap::with_capacity_and_hasher(10_000, Default::default()),
            blobs_fulu: FxHashMap::with_capacity_and_hasher(1_000, Default::default()),
        }
    }

    pub fn clear(&mut self) {
        self.transactions.clear();
        self.blobs_fulu.clear();
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

    pub fn builder_count(&self) -> usize {
        self.caches.len()
    }

    pub fn tx_count(&self) -> usize {
        self.caches.values().map(|c| c.transactions.len()).sum()
    }

    pub fn blob_count(&self) -> usize {
        self.caches.values().map(|c| c.blobs_fulu.len()).sum()
    }
}

impl Default for HydrationCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HydrationError {
    #[error("unkown tx: index {index}, hash {hash}")]
    UnknownTxHash { index: usize, hash: u64 },

    #[error("invalid tx bytes: length {length}, index {index}")]
    InvalidTxLength { length: usize, index: usize },

    #[error("unknown blob: commitment {commitment}, index {index}")]
    UnknownBlobHashFulu { commitment: KzgCommitment, index: usize },

    #[error("too many blobs: blobs {blobs}, max {max}")]
    TooManyBlobs { blobs: usize, max: usize },
}
