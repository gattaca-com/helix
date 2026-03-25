use std::{hash::Hasher, sync::Arc};

use alloy_primitives::{Address, B256, U256};
use lh_types::{ForkName, ForkVersionDecode};
use rustc_hash::{FxHashMap, FxHasher};
use serde::{Deserialize, Serialize};
use ssz::{Decode, DecodeError};
use ssz_derive::{Decode, Encode};
use tracing::trace;
use tree_hash::TreeHash;

use crate::{
    BidTrace, Blob, BlobsBundle, BlockValidationError, BlsPublicKeyBytes, BlsSignatureBytes,
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
            ForkName::Base |
            ForkName::Altair |
            ForkName::Bellatrix |
            ForkName::Capella |
            ForkName::Deneb |
            ForkName::Electra |
            ForkName::Gloas => Err(DecodeError::NoMatchingVariant),
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
            DehydratedBidSubmission::Fulu(s) => s.execution_payload.fee_recipient,
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

    pub fn fork_name(&self) -> lh_types::ForkName {
        match self {
            DehydratedBidSubmission::Fulu(_) => lh_types::ForkName::Fulu,
        }
    }

    pub fn timestamp(&self) -> u64 {
        match self {
            DehydratedBidSubmission::Fulu(s) => s.execution_payload.timestamp,
        }
    }

    pub fn prev_randao(&self) -> &B256 {
        match self {
            DehydratedBidSubmission::Fulu(s) => &s.execution_payload.prev_randao,
        }
    }

    pub fn proposer_fee_recipient(&self) -> &Address {
        match self {
            DehydratedBidSubmission::Fulu(s) => &s.message.proposer_fee_recipient,
        }
    }

    pub fn block_number(&self) -> u64 {
        match self {
            DehydratedBidSubmission::Fulu(s) => s.execution_payload.block_number,
        }
    }

    pub fn bid_trace(&self) -> &BidTrace {
        match self {
            DehydratedBidSubmission::Fulu(s) => &s.message,
        }
    }

    pub fn validate(&self) -> Result<(), BlockValidationError> {
        match self {
            DehydratedBidSubmission::Fulu(s) => {
                let msg = &s.message;
                let ep = &s.execution_payload;
                if msg.parent_hash != ep.parent_hash {
                    return Err(BlockValidationError::ParentHashMismatch {
                        message: msg.parent_hash,
                        payload: ep.parent_hash,
                    });
                }
                if msg.block_hash != ep.block_hash {
                    return Err(BlockValidationError::BlockHashMismatch {
                        message: msg.block_hash,
                        payload: ep.block_hash,
                    });
                }
                if msg.gas_limit != ep.gas_limit {
                    return Err(BlockValidationError::GasLimitMismatch {
                        message: msg.gas_limit,
                        payload: ep.gas_limit,
                    });
                }
                if msg.gas_used != ep.gas_used {
                    return Err(BlockValidationError::GasUsedMismatch {
                        message: msg.gas_used,
                        payload: ep.gas_used,
                    });
                }
                if msg.value == U256::ZERO {
                    return Err(BlockValidationError::ZeroValueBlock);
                }
                Ok(())
            }
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
            ForkName::Base |
            ForkName::Altair |
            ForkName::Bellatrix |
            ForkName::Capella |
            ForkName::Deneb |
            ForkName::Gloas |
            ForkName::Electra => Err(DecodeError::NoMatchingVariant),
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
    fn can_hydrate_inner(
        &self,
        txs: &FxHashMap<u64, Transaction>,
        blobs: &FxHashMap<KzgCommitment, (Vec<KzgProof>, Blob)>,
        max_blobs_per_block: usize,
    ) -> bool {
        if self.blobs_bundle.commitments.len() > max_blobs_per_block {
            return false;
        }

        /// Max size of the rlp encoded tx sig (v 1 byte, s,r 32 bytes each with a leading
        /// rlp length prefix)
        const TX_KEY_SIZE: usize = 67;

        for tx in &self.execution_payload.transactions {
            if tx.len() == std::mem::size_of::<u64>() {
                let bytes = tx.as_ref().try_into().unwrap();
                let hash = u64::from_le_bytes(bytes);
                if !txs.contains_key(&hash) {
                    return false;
                }
            } else if tx.len() < TX_KEY_SIZE {
                return false;
            }
        }

        for commitment in &self.blobs_bundle.commitments {
            if !blobs.contains_key(commitment) &&
                !self.blobs_bundle.new_items.iter().any(|b| &b.commitment == commitment)
            {
                return false;
            }
        }

        true
    }

    fn hydrate_inner(
        mut self,
        txs: &mut FxHashMap<u64, Transaction>,
        blobs: &mut FxHashMap<KzgCommitment, (Vec<KzgProof>, Blob)>,
        max_blobs_per_block: usize,
    ) -> Result<HydratedData, HydrationError> {
        // avoid short-circuiting the loop to maximize cache population
        let mut last_err = Ok(());

        // hydrate transactions

        let mut tx_cache_hits = 0;

        for (index, tx) in self.execution_payload.transactions.iter_mut().enumerate() {
            if tx.len() == std::mem::size_of::<u64>() {
                // hashed transaction, hydrate it
                let bytes = tx.as_ref().try_into().unwrap();
                let hash = u64::from_le_bytes(bytes);
                let Some(cached_tx) = txs.get(&hash) else {
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
                txs.insert(hash, tx.clone());
                trace!("Inserted tx into cache: index {}, hash {}", index, hash);
            };
        }

        // hydrate blobs

        let mut blob_cache_hits: usize = 0;
        let new_blobs = self.blobs_bundle.new_items.len();
        for blob_item in self.blobs_bundle.new_items {
            blobs.insert(blob_item.commitment, (blob_item.proof, blob_item.blob));
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
            let Some((proofs, blob)) = blobs.get(&commitment) else {
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
            blobs_bundle: Arc::new(sidecar),
            execution_requests: self.execution_requests,
            signature: self.signature,
        };

        Ok(HydratedData { submission, tx_cache_hits, blob_cache_hits, tx_root: self.tx_root })
    }
}

struct Cache {
    transactions: FxHashMap<u64, Transaction>,
    blobs_fulu: FxHashMap<KzgCommitment, (Vec<KzgProof>, Blob)>,
}

impl Cache {
    fn new() -> Self {
        Self {
            transactions: FxHashMap::with_capacity_and_hasher(10_000, Default::default()),
            blobs_fulu: FxHashMap::with_capacity_and_hasher(1_000, Default::default()),
        }
    }

    fn clear(&mut self) {
        self.transactions.clear();
        self.blobs_fulu.clear();
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SimHydrationCache {
    transactions: FxHashMap<u64, Transaction>,
    blobs_fulu: FxHashMap<KzgCommitment, (Vec<KzgProof>, Blob)>,
}

impl SimHydrationCache {
    pub fn new() -> Self {
        Self {
            transactions: FxHashMap::with_capacity_and_hasher(10_000, Default::default()),
            blobs_fulu: FxHashMap::with_capacity_and_hasher(1_000, Default::default()),
        }
    }

    pub fn can_hydrate(
        &self,
        submission: &DehydratedBidSubmission,
        max_blobs_per_block: usize,
    ) -> bool {
        match submission {
            DehydratedBidSubmission::Fulu(s) => {
                s.can_hydrate_inner(&self.transactions, &self.blobs_fulu, max_blobs_per_block)
            }
        }
    }

    pub fn hydrate(
        &mut self,
        submission: DehydratedBidSubmission,
        max_blobs_per_block: usize,
    ) -> Result<HydratedData, HydrationError> {
        match submission {
            DehydratedBidSubmission::Fulu(s) => {
                s.hydrate_inner(&mut self.transactions, &mut self.blobs_fulu, max_blobs_per_block)
            }
        }
    }

    pub fn clear(&mut self) {
        self.transactions.clear();
        self.blobs_fulu.clear();
    }

    pub fn tx_count(&self) -> usize {
        self.transactions.len()
    }

    pub fn blob_count(&self) -> usize {
        self.blobs_fulu.len()
    }
}

impl Default for SimHydrationCache {
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

    pub fn can_hydrate(
        &self,
        submission: &DehydratedBidSubmission,
        max_blobs_per_block: usize,
    ) -> bool {
        match submission {
            DehydratedBidSubmission::Fulu(s) => {
                match self.caches.get(&s.message.builder_pubkey) {
                    Some(cache) => s.can_hydrate_inner(&cache.transactions, &cache.blobs_fulu, max_blobs_per_block),
                    None => false,
                }
            }
        }
    }

    pub fn hydrate(
        &mut self,
        submission: DehydratedBidSubmission,
        max_blobs_per_block: usize,
    ) -> Result<HydratedData, HydrationError> {
        match submission {
            DehydratedBidSubmission::Fulu(s) => {
                let pubkey = s.message.builder_pubkey;
                let Some(entry) = self.caches.get_mut(&pubkey) else {
                    return Err(HydrationError::MissingBuilderCache { pubkey });
                };
                s.hydrate_inner(&mut entry.transactions, &mut entry.blobs_fulu, max_blobs_per_block)
            }
        }
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

    #[error("no cache for builder {pubkey}")]
    MissingBuilderCache { pubkey: BlsPublicKeyBytes },
}
