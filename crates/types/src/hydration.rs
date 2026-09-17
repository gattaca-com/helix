use std::{hash::Hasher, sync::Arc};

use alloy_primitives::{Address, B256, U256};
use flux_profiler::timed;
use lh_types::{ForkName, ForkVersionDecode};
use rustc_hash::{FxHashMap, FxHasher};
use serde::{Deserialize, Serialize};
use ssz::{Decode, DecodeError};
use ssz_derive::{Decode, Encode};
use tracing::trace;
use tree_hash::TreeHash;

use crate::{
    BidTrace, Blob, BlobsBundle, BlockMergingData, BlockValidationError, BlsPublicKeyBytes,
    BlsSignatureBytes, ExecutionPayload, SignedBidSubmission, TestRandom,
    bid_adjustment_data::{BidAdjData, BidAdjustmentData, BidAdjustmentDataV1},
    bid_submission,
    fields::{ExecutionRequests, KzgCommitment, KzgProof, Transaction, Transactions},
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
            ForkName::Gloas |
            ForkName::Heze => Err(DecodeError::NoMatchingVariant),
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

    pub fn num_txs(&self) -> usize {
        match self {
            DehydratedBidSubmission::Fulu(s) if s.tx_refs.is_empty() => {
                s.execution_payload.transactions.len()
            }
            DehydratedBidSubmission::Fulu(s) => s.tx_refs.len(),
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
    /// Dehydration v2 only, never on the v1 wire: block order as cache keys,
    /// `NEXT_FULL_ITEM` for the next entry of `transactions` / `new_items`.
    /// Empty under v1, where refs sit inline as 8-byte transactions.
    #[ssz(skip_serializing, skip_deserializing)]
    #[serde(skip)]
    tx_refs: Vec<u64>,
    #[ssz(skip_serializing, skip_deserializing)]
    #[serde(skip)]
    blob_refs: Vec<u64>,
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
                tx_refs: Vec::new(),
                blob_refs: Vec::new(),
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
            ForkName::Heze |
            ForkName::Electra => Err(DecodeError::NoMatchingVariant),
            ForkName::Fulu => DehydratedBidSubmissionFuluWithAdjustments::from_ssz_bytes(bytes),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct DehydratedBidSubmissionFuluWithMergingData {
    message: BidTrace,
    execution_payload: ExecutionPayload,
    blobs_bundle: DehydratedBlobsFulu,
    execution_requests: Arc<ExecutionRequests>,
    signature: BlsSignatureBytes,
    tx_root: Option<B256>,
    merging_data: BlockMergingData,
}

impl DehydratedBidSubmissionFuluWithMergingData {
    pub fn split(self) -> (DehydratedBidSubmission, BlockMergingData) {
        (
            DehydratedBidSubmission::Fulu(DehydratedBidSubmissionFulu {
                message: self.message,
                execution_payload: self.execution_payload,
                blobs_bundle: self.blobs_bundle,
                execution_requests: self.execution_requests,
                signature: self.signature,
                tx_root: self.tx_root,
                tx_refs: Vec::new(),
                blob_refs: Vec::new(),
            }),
            self.merging_data,
        )
    }
}

impl ForkVersionDecode for DehydratedBidSubmissionFuluWithMergingData {
    fn from_ssz_bytes_by_fork(bytes: &[u8], fork: ForkName) -> Result<Self, DecodeError> {
        match fork {
            ForkName::Base |
            ForkName::Altair |
            ForkName::Bellatrix |
            ForkName::Capella |
            ForkName::Deneb |
            ForkName::Gloas |
            ForkName::Heze |
            ForkName::Electra => Err(DecodeError::NoMatchingVariant),
            ForkName::Fulu => DehydratedBidSubmissionFuluWithMergingData::from_ssz_bytes(bytes),
        }
    }
}

impl TestRandom for DehydratedBidSubmissionFuluWithMergingData {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            message: BidTrace::random_for_test(rng),
            execution_payload: ExecutionPayload::random_for_test(rng),
            blobs_bundle: DehydratedBlobsFulu { commitments: vec![], new_items: vec![] },
            execution_requests: Arc::new(ExecutionRequests::random_for_test(rng)),
            signature: BlsSignatureBytes::random(),
            tx_root: None,
            merging_data: BlockMergingData::random_for_test(rng),
        }
    }
}

/// Dehydration v2 (TCP only). `transactions` and `new_items` carry only what
/// the relay has not seen; `tx_refs` / `blobs_bundle.refs` give block order as
/// fixed 8-byte cache keys, [`NEXT_FULL_ITEM`] meaning the next full entry.
/// Field order is mirrored by the builder's encoder.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct DehydratedBidSubmissionFuluV2 {
    message: BidTrace,
    execution_payload: ExecutionPayload,
    blobs_bundle: DehydratedBlobsFuluV2,
    execution_requests: Arc<ExecutionRequests>,
    signature: BlsSignatureBytes,
    tx_root: Option<B256>,
    tx_refs: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct DehydratedBlobsFuluV2 {
    refs: Vec<u64>,
    new_items: Vec<BlobItemFulu>,
}

/// v2 ref value for "the next full transaction / new blob".
pub const NEXT_FULL_ITEM: u64 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidDehydratedV2 {
    #[error("{refs} next-full tx refs for {full} full txs")]
    TxRefs { refs: usize, full: usize },
    #[error("{refs} next-full blob refs for {new} new blobs")]
    BlobRefs { refs: usize, new: usize },
}

impl TryFrom<DehydratedBidSubmissionFuluV2> for DehydratedBidSubmission {
    type Error = InvalidDehydratedV2;

    /// Field moves only; the v1 shape hydrates straight from the refs.
    fn try_from(v2: DehydratedBidSubmissionFuluV2) -> Result<Self, Self::Error> {
        let full = v2.execution_payload.transactions.len();
        let refs = v2.tx_refs.iter().filter(|&&r| r == NEXT_FULL_ITEM).count();
        if refs != full {
            return Err(InvalidDehydratedV2::TxRefs { refs, full });
        }
        let new = v2.blobs_bundle.new_items.len();
        let refs = v2.blobs_bundle.refs.iter().filter(|&&r| r == NEXT_FULL_ITEM).count();
        if refs != new {
            return Err(InvalidDehydratedV2::BlobRefs { refs, new });
        }
        Ok(DehydratedBidSubmission::Fulu(DehydratedBidSubmissionFulu {
            message: v2.message,
            execution_payload: v2.execution_payload,
            blobs_bundle: DehydratedBlobsFulu {
                commitments: Vec::new(),
                new_items: v2.blobs_bundle.new_items,
            },
            execution_requests: v2.execution_requests,
            signature: v2.signature,
            tx_root: v2.tx_root,
            tx_refs: v2.tx_refs,
            blob_refs: v2.blobs_bundle.refs,
        }))
    }
}

impl From<DehydratedBidSubmissionFulu> for DehydratedBidSubmission {
    fn from(s: DehydratedBidSubmissionFulu) -> Self {
        DehydratedBidSubmission::Fulu(s)
    }
}

impl ForkVersionDecode for DehydratedBidSubmissionFuluV2 {
    fn from_ssz_bytes_by_fork(bytes: &[u8], fork: ForkName) -> Result<Self, DecodeError> {
        match fork {
            ForkName::Fulu => DehydratedBidSubmissionFuluV2::from_ssz_bytes(bytes),
            _ => Err(DecodeError::NoMatchingVariant),
        }
    }
}

/// `[submission][merging_data]` container for the v2 TCP shapes, so each
/// submission and merging encoding pairs without a struct per combination.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct WithMergingData<S: ssz::Encode + ssz::Decode, M: ssz::Encode + ssz::Decode> {
    pub submission: S,
    pub merging_data: M,
}

impl<S: ssz::Encode + ssz::Decode, M: ssz::Encode + ssz::Decode> ForkVersionDecode
    for WithMergingData<S, M>
{
    fn from_ssz_bytes_by_fork(bytes: &[u8], fork: ForkName) -> Result<Self, DecodeError> {
        match fork {
            ForkName::Fulu => Self::from_ssz_bytes(bytes),
            _ => Err(DecodeError::NoMatchingVariant),
        }
    }
}

impl TestRandom for DehydratedBidSubmissionFuluV2 {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        let mut execution_payload = ExecutionPayload::random_for_test(rng);
        execution_payload.transactions =
            Transactions::new((1..=5).map(full_tx_for_test).collect()).unwrap();
        Self {
            message: BidTrace::random_for_test(rng),
            blobs_bundle: DehydratedBlobsFuluV2 { refs: vec![], new_items: vec![] },
            execution_requests: Arc::new(ExecutionRequests::random_for_test(rng)),
            signature: BlsSignatureBytes::random(),
            tx_root: None,
            tx_refs: vec![NEXT_FULL_ITEM; execution_payload.transactions.len()],
            execution_payload,
        }
    }
}

impl<S: ssz::Encode + ssz::Decode + TestRandom, M: ssz::Encode + ssz::Decode + TestRandom>
    TestRandom for WithMergingData<S, M>
{
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self { submission: S::random_for_test(rng), merging_data: M::random_for_test(rng) }
    }
}

/// Flat combination of [`DehydratedBidSubmissionFuluWithAdjustments`] and merging data:
/// core fields ++ bid_adjustment_data ++ merging_data.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct DehydratedBidSubmissionFuluWithAdjustmentsAndMergingData {
    message: BidTrace,
    execution_payload: ExecutionPayload,
    blobs_bundle: DehydratedBlobsFulu,
    execution_requests: Arc<ExecutionRequests>,
    signature: BlsSignatureBytes,
    tx_root: Option<B256>,
    bid_adjustment_data: BidAdjustmentData,
    merging_data: BlockMergingData,
}

impl DehydratedBidSubmissionFuluWithAdjustmentsAndMergingData {
    pub fn split(self) -> (DehydratedBidSubmission, BidAdjustmentData, BlockMergingData) {
        (
            DehydratedBidSubmission::Fulu(DehydratedBidSubmissionFulu {
                message: self.message,
                execution_payload: self.execution_payload,
                blobs_bundle: self.blobs_bundle,
                execution_requests: self.execution_requests,
                signature: self.signature,
                tx_root: self.tx_root,
                tx_refs: Vec::new(),
                blob_refs: Vec::new(),
            }),
            self.bid_adjustment_data,
            self.merging_data,
        )
    }
}

impl ForkVersionDecode for DehydratedBidSubmissionFuluWithAdjustmentsAndMergingData {
    fn from_ssz_bytes_by_fork(bytes: &[u8], fork: ForkName) -> Result<Self, DecodeError> {
        match fork {
            ForkName::Base |
            ForkName::Altair |
            ForkName::Bellatrix |
            ForkName::Capella |
            ForkName::Deneb |
            ForkName::Gloas |
            ForkName::Heze |
            ForkName::Electra => Err(DecodeError::NoMatchingVariant),
            ForkName::Fulu => {
                DehydratedBidSubmissionFuluWithAdjustmentsAndMergingData::from_ssz_bytes(bytes)
            }
        }
    }
}

impl TestRandom for DehydratedBidSubmissionFuluWithAdjustmentsAndMergingData {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            message: BidTrace::random_for_test(rng),
            execution_payload: ExecutionPayload::random_for_test(rng),
            blobs_bundle: DehydratedBlobsFulu { commitments: vec![], new_items: vec![] },
            execution_requests: Arc::new(ExecutionRequests::random_for_test(rng)),
            signature: BlsSignatureBytes::random(),
            tx_root: None,
            bid_adjustment_data: BidAdjustmentData::V1(BidAdjustmentDataV1::Original(
                BidAdjData::default(),
            )),
            merging_data: BlockMergingData::random_for_test(rng),
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

/// Max size of the rlp encoded tx sig (v 1 byte, s,r 32 bytes each with a leading
/// rlp length prefix)
const TX_KEY_SIZE: usize = 67;

type CachedBlob = (KzgCommitment, Vec<KzgProof>, Blob);

/// Blob cache key: FxHash of the 48-byte commitment. Mirrored by the builder
/// for v2 `blob_refs`.
pub fn blob_cache_key(commitment: &KzgCommitment) -> u64 {
    let mut hasher = FxHasher::default();
    hasher.write(commitment.as_slice());
    hasher.finish()
}

impl DehydratedBidSubmissionFulu {
    fn num_blobs(&self) -> usize {
        if self.blob_refs.is_empty() {
            self.blobs_bundle.commitments.len()
        } else {
            self.blob_refs.len()
        }
    }

    #[timed]
    fn can_hydrate_inner(
        &self,
        txs: &FxHashMap<u64, Transaction>,
        blobs: &FxHashMap<u64, CachedBlob>,
        max_blobs_per_block: usize,
    ) -> bool {
        if self.num_blobs() > max_blobs_per_block {
            return false;
        }

        if self.tx_refs.is_empty() {
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
        } else {
            if self.tx_refs.iter().any(|&r| r != NEXT_FULL_ITEM && !txs.contains_key(&r)) {
                return false;
            }
            if self.execution_payload.transactions.iter().any(|tx| tx.len() < TX_KEY_SIZE) {
                return false;
            }
        }

        if self.blob_refs.is_empty() {
            for commitment in &self.blobs_bundle.commitments {
                if !blobs.contains_key(&blob_cache_key(commitment)) &&
                    !self.blobs_bundle.new_items.iter().any(|b| &b.commitment == commitment)
                {
                    return false;
                }
            }
        } else if self.blob_refs.iter().any(|&r| r != NEXT_FULL_ITEM && !blobs.contains_key(&r)) {
            return false;
        }

        true
    }

    #[timed]
    fn hydrate_inner(
        mut self,
        txs: &mut FxHashMap<u64, Transaction>,
        blobs: &mut FxHashMap<u64, CachedBlob>,
        max_blobs_per_block: usize,
    ) -> Result<HydratedData, HydrationError> {
        // avoid short-circuiting the loop to maximize cache population
        let mut last_err = Ok(());
        let mut tx_cache_hits = 0;

        if self.tx_refs.is_empty() {
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
                    if tx.len() < TX_KEY_SIZE {
                        last_err = Err(HydrationError::InvalidTxLength { length: tx.len(), index });
                        continue;
                    }

                    let hash = tx_cache_key(tx);
                    txs.insert(hash, tx.clone());
                    trace!("Inserted tx into cache: index {}, hash {}", index, hash);
                };
            }
        } else {
            // v2: same work as above, into a fresh list instead of in place.
            let full: Vec<Transaction> =
                std::mem::take(&mut self.execution_payload.transactions).into();
            let mut full = full.into_iter();
            let mut hydrated = Vec::with_capacity(self.tx_refs.len());
            for (index, &tx_ref) in self.tx_refs.iter().enumerate() {
                if tx_ref == NEXT_FULL_ITEM {
                    let Some(tx) = full.next() else {
                        last_err = Err(HydrationError::MissingFullTx { index });
                        continue;
                    };
                    if tx.len() < TX_KEY_SIZE {
                        last_err = Err(HydrationError::InvalidTxLength { length: tx.len(), index });
                    } else {
                        txs.insert(tx_cache_key(&tx), tx.clone());
                    }
                    hydrated.push(tx);
                } else if let Some(cached_tx) = txs.get(&tx_ref) {
                    tx_cache_hits += 1;
                    hydrated.push(cached_tx.clone());
                } else {
                    last_err = Err(HydrationError::UnknownTxHash { index, hash: tx_ref });
                }
            }
            self.execution_payload.transactions =
                Transactions::new(hydrated).map_err(|_| HydrationError::TooManyTxs)?;
        }

        // hydrate blobs

        let mut blob_cache_hits: usize = 0;
        let new_blobs = self.blobs_bundle.new_items.len();
        let mut new_keys =
            Vec::with_capacity(if self.blob_refs.is_empty() { 0 } else { new_blobs });
        for blob_item in self.blobs_bundle.new_items {
            let key = blob_cache_key(&blob_item.commitment);
            if !self.blob_refs.is_empty() {
                new_keys.push(key);
            }
            blobs.insert(key, (blob_item.commitment, blob_item.proof, blob_item.blob));
        }

        let num_blobs = if self.blob_refs.is_empty() {
            self.blobs_bundle.commitments.len()
        } else {
            self.blob_refs.len()
        };
        if num_blobs > max_blobs_per_block {
            last_err =
                Err(HydrationError::TooManyBlobs { blobs: num_blobs, max: max_blobs_per_block });
        }

        last_err?;

        let mut sidecar = BlobsBundle::with_capacity(num_blobs);
        let mut next_new = 0;
        for index in 0..num_blobs {
            let key = if self.blob_refs.is_empty() {
                blob_cache_key(&self.blobs_bundle.commitments[index])
            } else if self.blob_refs[index] == NEXT_FULL_ITEM {
                next_new += 1;
                new_keys[next_new - 1]
            } else {
                self.blob_refs[index]
            };
            let Some((commitment, proofs, blob)) = blobs.get(&key) else {
                return Err(HydrationError::UnknownBlobHashFulu { key, index });
            };

            // safe because we checked the length above
            sidecar.commitments.push(*commitment).unwrap();
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

    /// Inserts this submission's full transactions and new blobs into the
    /// cache without building the hydrated payload, so later dehydrated
    /// submissions can resolve their references even when this one is
    /// otherwise dropped.
    fn feed_inner(
        &self,
        txs: &mut FxHashMap<u64, Transaction>,
        blobs: &mut FxHashMap<u64, CachedBlob>,
    ) {
        for tx in &self.execution_payload.transactions {
            if tx.len() == std::mem::size_of::<u64>() || tx.len() < TX_KEY_SIZE {
                continue;
            }
            txs.insert(tx_cache_key(tx), tx.clone());
        }
        for blob_item in &self.blobs_bundle.new_items {
            blobs.insert(
                blob_cache_key(&blob_item.commitment),
                (blob_item.commitment, blob_item.proof.clone(), blob_item.blob.clone()),
            );
        }
    }
}

struct Cache {
    transactions: FxHashMap<u64, Transaction>,
    blobs_fulu: FxHashMap<u64, CachedBlob>,
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
    blobs_fulu: FxHashMap<u64, CachedBlob>,
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

    /// Inserts the submission's full transactions and new blobs into the cache
    /// without building the hydrated payload, so a submission the sim tile will
    /// not hydrate still lets later submissions resolve their references.
    pub fn feed(&mut self, submission: &DehydratedBidSubmission) {
        match submission {
            DehydratedBidSubmission::Fulu(s) => {
                s.feed_inner(&mut self.transactions, &mut self.blobs_fulu);
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
                let empty_txs = FxHashMap::default();
                let empty_blobs = FxHashMap::default();
                let (txs, blobs) = self
                    .caches
                    .get(&s.message.builder_pubkey)
                    .map(|c| (&c.transactions, &c.blobs_fulu))
                    .unwrap_or((&empty_txs, &empty_blobs));
                s.can_hydrate_inner(txs, blobs, max_blobs_per_block)
            }
        }
    }

    #[timed]
    pub fn hydrate(
        &mut self,
        submission: DehydratedBidSubmission,
        max_blobs_per_block: usize,
    ) -> Result<HydratedData, HydrationError> {
        match submission {
            DehydratedBidSubmission::Fulu(s) => {
                let pubkey = s.message.builder_pubkey;
                let entry = self.caches.entry(pubkey).or_default();
                s.hydrate_inner(&mut entry.transactions, &mut entry.blobs_fulu, max_blobs_per_block)
            }
        }
    }

    /// Inserts the submission's full transactions and new blobs into the
    /// builder's cache without building the hydrated payload.
    pub fn feed(&mut self, submission: &DehydratedBidSubmission) {
        match submission {
            DehydratedBidSubmission::Fulu(s) => {
                let entry = self.caches.entry(s.message.builder_pubkey).or_default();
                s.feed_inner(&mut entry.transactions, &mut entry.blobs_fulu);
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
    #[error("unknown tx: index {index}, hash {hash}")]
    UnknownTxHash { index: usize, hash: u64 },

    #[error("invalid tx bytes: length {length}, index {index}")]
    InvalidTxLength { length: usize, index: usize },

    #[error("unknown blob: key {key}, index {index}")]
    UnknownBlobHashFulu { key: u64, index: usize },

    #[error("too many blobs: blobs {blobs}, max {max}")]
    TooManyBlobs { blobs: usize, max: usize },

    #[error("tx ref {index} expects a full tx but none remain")]
    MissingFullTx { index: usize },

    #[error("too many txs")]
    TooManyTxs,
}

/// Test helpers. These live here because `DehydratedBidSubmissionFulu`'s fields are
/// private to this module, and the relay's tile tests need control over which
/// transactions a submission carries in full and which it carries by reference.
/// A transaction long enough to be keyed, rather than read as a hash reference.
pub fn full_tx_for_test(fill: u8) -> Transaction {
    Transaction(vec![fill; TX_KEY_SIZE + 5].into())
}

/// The cache key a full transaction is stored under.
pub fn tx_cache_key(tx: &Transaction) -> u64 {
    let mut hasher = FxHasher::default();
    hasher.write(&tx[tx.len() - TX_KEY_SIZE..]);
    hasher.finish()
}

/// The 8-byte stand-in a builder sends once the relay has seen the full transaction.
pub fn tx_hash_ref_for_test(key: u64) -> Transaction {
    Transaction(key.to_le_bytes().to_vec().into())
}

impl DehydratedBidSubmission {
    /// Overrides the builder, so a test can make two submissions share one.
    pub fn set_builder_pubkey_for_test(&mut self, pubkey: BlsPublicKeyBytes) {
        match self {
            DehydratedBidSubmission::Fulu(s) => s.message.builder_pubkey = pubkey,
        }
    }
}

/// A dehydrated submission whose payload holds exactly `txs` and no blobs.
pub fn dehydrated_submission_with_txs_for_test(txs: Vec<Transaction>) -> DehydratedBidSubmission {
    let (dehydrated, _) =
        DehydratedBidSubmissionFuluWithMergingData::random_for_test(&mut rand::rng()).split();
    let DehydratedBidSubmission::Fulu(mut s) = dehydrated;
    s.execution_payload.transactions = Transactions::new(txs).expect("tx list within limits");
    s.blobs_bundle = DehydratedBlobsFulu { commitments: vec![], new_items: vec![] };
    DehydratedBidSubmission::Fulu(s)
}

#[cfg(test)]
mod tests {
    use ssz::Encode;

    use super::*;

    const MAX_BLOBS: usize = 9;

    fn full_tx(fill: u8) -> Transaction {
        full_tx_for_test(fill)
    }

    fn tx_key(tx: &Transaction) -> u64 {
        tx_cache_key(tx)
    }

    fn tx_ref(key: u64) -> Transaction {
        tx_hash_ref_for_test(key)
    }

    fn submission_with(txs: Vec<Transaction>) -> DehydratedBidSubmission {
        dehydrated_submission_with_txs_for_test(txs)
    }

    /// Documents the failure behind gattaca-com/helix#537: the sim tile's cache is
    /// populated only by `hydrate`, so a submission that arrived and was queued
    /// without being simulated leaves no trace, and the next submission that
    /// references its transactions cannot be hydrated.
    #[test]
    fn sim_cache_misses_when_the_earlier_submission_was_only_queued() {
        let tx = full_tx(1);
        let earlier = submission_with(vec![tx.clone()]);
        let later = submission_with(vec![tx_ref(tx_key(&tx))]);

        let mut cache = SimHydrationCache::new();

        assert!(!cache.can_hydrate(&later, MAX_BLOBS));
        assert!(matches!(
            cache.hydrate(later, MAX_BLOBS),
            Err(HydrationError::UnknownTxHash { .. })
        ));

        // Building the earlier payload is currently the only way to fill the cache.
        assert!(cache.hydrate(earlier, MAX_BLOBS).is_ok());
    }

    /// `feed` must fill the cache from a submission the tile is not going to
    /// hydrate, so arrival order stops deciding whether a later submission can be
    /// simulated. Mirrors `HydrationCache::feed`, used by the merging tile.
    #[test]
    fn sim_cache_feed_enables_later_hydration_without_building_a_payload() {
        let tx = full_tx(2);
        let earlier = submission_with(vec![tx.clone()]);
        let later = submission_with(vec![tx_ref(tx_key(&tx))]);

        let mut cache = SimHydrationCache::new();
        cache.feed(&earlier);

        assert_eq!(cache.tx_count(), 1);
        assert!(cache.can_hydrate(&later, MAX_BLOBS));

        let hydrated = cache.hydrate(later, MAX_BLOBS).expect("hydration should succeed");
        assert_eq!(hydrated.tx_cache_hits, 1, "the reference must resolve from the cache");
        assert_eq!(hydrated.submission.execution_payload.transactions.len(), 1);
    }

    /// Feeding the same submission twice must not change what the cache holds,
    /// since every arriving submission is fed and retries re-send the same bid.
    #[test]
    fn sim_cache_feed_is_idempotent() {
        let tx = full_tx(3);
        let submission = submission_with(vec![tx.clone()]);

        let mut cache = SimHydrationCache::new();
        cache.feed(&submission);
        cache.feed(&submission);

        assert_eq!(cache.tx_count(), 1);
    }

    /// A hash reference carries no transaction bytes, so feeding a submission that
    /// holds only references must add nothing.
    #[test]
    fn sim_cache_feed_ignores_hash_references() {
        let tx = full_tx(4);
        let only_refs = submission_with(vec![tx_ref(tx_key(&tx))]);

        let mut cache = SimHydrationCache::new();
        cache.feed(&only_refs);

        assert_eq!(cache.tx_count(), 0);
    }

    #[test]
    fn dehydrated_with_merging_data_ssz_round_trip() {
        let submission =
            DehydratedBidSubmissionFuluWithMergingData::random_for_test(&mut rand::rng());

        let bytes = submission.as_ssz_bytes();
        let decoded = DehydratedBidSubmissionFuluWithMergingData::from_ssz_bytes(&bytes)
            .expect("SSZ decode should succeed");

        assert_eq!(submission.merging_data, decoded.merging_data);
        assert_eq!(submission.message, decoded.message);
    }

    #[test]
    fn dehydrated_with_merging_data_split_preserves_merge_orders() {
        let submission =
            DehydratedBidSubmissionFuluWithMergingData::random_for_test(&mut rand::rng());
        let expected_merging_data = submission.merging_data.clone();

        let (dehydrated, split_merging_data) = submission.split();

        assert_eq!(split_merging_data, expected_merging_data);
        assert!(matches!(dehydrated, DehydratedBidSubmission::Fulu(_)));
    }

    /// A v2 send hydrates from, and feeds, the same cache as v1: refs resolve
    /// by key and full items are inserted under their key.
    #[test]
    fn dehydrated_v2_hydrates_from_refs() {
        let mut v2 = DehydratedBidSubmissionFuluV2::random_for_test(&mut rand::rng());
        let (a, b) = (full_tx_for_test(0xa), full_tx_for_test(0xb));
        let earlier = submission_with(vec![a.clone()]);
        let mut cache = HydrationCache::new();
        cache.feed(&earlier);

        v2.message.builder_pubkey = *earlier.builder_pubkey();
        v2.execution_payload.transactions = Transactions::new(vec![b.clone()]).unwrap();
        v2.tx_refs = vec![tx_cache_key(&a), NEXT_FULL_ITEM, tx_cache_key(&a)];
        let dehydrated = DehydratedBidSubmission::try_from(v2).unwrap();
        assert_eq!(dehydrated.num_txs(), 3);
        assert!(cache.can_hydrate(&dehydrated, 6));

        let hydrated = cache.hydrate(dehydrated, 6).unwrap();
        let txs: Vec<Transaction> =
            hydrated.submission.execution_payload.transactions.iter().cloned().collect();
        assert_eq!(txs, vec![a.clone(), b.clone(), a]);
        assert_eq!(hydrated.tx_cache_hits, 2);

        let later = submission_with(vec![tx_hash_ref_for_test(tx_cache_key(&b))]);
        let mut later = later;
        later.set_builder_pubkey_for_test(*earlier.builder_pubkey());
        assert!(cache.can_hydrate(&later, 6), "v2 full tx was cached under its key");
    }

    #[test]
    fn dehydrated_v2_rejects_ref_count_mismatch() {
        let mut v2 = DehydratedBidSubmissionFuluV2::random_for_test(&mut rand::rng());
        v2.tx_refs.push(NEXT_FULL_ITEM);
        let full = v2.execution_payload.transactions.len();
        assert_eq!(
            DehydratedBidSubmission::try_from(v2).unwrap_err(),
            InvalidDehydratedV2::TxRefs { refs: full + 1, full }
        );
    }
}
