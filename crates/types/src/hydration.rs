use std::{hash::Hasher, sync::Arc};

use alloy_primitives::{Address, B256, U256};
use flux_profiler::timed;
use lh_types::{ForkName, ForkVersionDecode, Withdrawal};
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
    fields::{ExecutionRequests, KzgCommitment, KzgProof, Transaction, Transactions, Withdrawals},
};

/// A bid submission where transactions and blobs may be replaced by hashes instead of payload
#[derive(Debug, Clone, Deserialize)]
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
            ForkName::Fulu => DehydratedBidSubmissionFuluV1::from_ssz_bytes(bytes)
                .map(|v1| DehydratedBidSubmission::Fulu(v1.into())),
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

/// v1 wire shape (HTTP JSON/SSZ and unflagged TCP): a cached tx is its 8-byte
/// key inline in `transactions`, a cached blob its commitment. Converted into
/// [`DehydratedBidSubmissionFulu`] on decode.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct DehydratedBidSubmissionFuluV1 {
    message: BidTrace,
    execution_payload: ExecutionPayload,
    blobs_bundle: DehydratedBlobsFulu,
    execution_requests: Arc<ExecutionRequests>,
    signature: BlsSignatureBytes,
    tx_root: Option<B256>,
}

/// The relay's dehydrated submission and the v2 TCP wire shape. `transactions`
/// and `blobs_bundle.new_items` hold only what the relay has not seen;
/// `tx_refs` and `blobs_bundle.refs` give block order as 8-byte cache keys,
/// [`NEXT_FULL_ITEM`] meaning the next full entry. Field order is mirrored by
/// the builder's encoder.
#[derive(Debug, Clone, Deserialize, Encode, Decode)]
#[serde(from = "DehydratedBidSubmissionFuluV1")]
pub struct DehydratedBidSubmissionFulu {
    message: BidTrace,
    execution_payload: ExecutionPayload,
    blobs_bundle: DehydratedBlobsFuluV2,
    execution_requests: Arc<ExecutionRequests>,
    signature: BlsSignatureBytes,
    tx_root: Option<B256>,
    tx_refs: Vec<u64>,
    /// Key of the slot's withdrawals if the relay has them, else 0 and
    /// `execution_payload.withdrawals` is inline.
    withdrawals_ref: u64,
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct DehydratedBlobsFuluV2 {
    refs: Vec<u64>,
    new_items: Vec<BlobItemFulu>,
}

/// Ref value for "the next full transaction / new blob".
pub const NEXT_FULL_ITEM: u64 = 0;

impl From<DehydratedBidSubmissionFuluV1> for DehydratedBidSubmissionFulu {
    /// One pass over the tx list: keys become refs, full txs move to the
    /// front. Blob refs are the commitment hashes; a v1 new blob is looked up
    /// by hash too, so `0` never appears.
    fn from(mut v1: DehydratedBidSubmissionFuluV1) -> Self {
        let txs: Vec<Transaction> = std::mem::take(&mut v1.execution_payload.transactions).into();
        let n_full = txs.iter().filter(|tx| tx.len() != std::mem::size_of::<u64>()).count();
        let mut tx_refs = Vec::with_capacity(txs.len());
        let mut full = Vec::with_capacity(n_full);
        for tx in txs {
            if tx.len() == std::mem::size_of::<u64>() {
                tx_refs.push(u64::from_le_bytes(tx.as_ref().try_into().unwrap()));
            } else {
                tx_refs.push(NEXT_FULL_ITEM);
                full.push(tx);
            }
        }
        let mut execution_payload = v1.execution_payload;
        execution_payload.transactions =
            Transactions::new(full).expect("no longer than the v1 list");
        Self {
            message: v1.message,
            execution_payload,
            blobs_bundle: DehydratedBlobsFuluV2 {
                refs: v1.blobs_bundle.commitments.iter().map(blob_cache_key).collect(),
                new_items: v1.blobs_bundle.new_items,
            },
            execution_requests: v1.execution_requests,
            signature: v1.signature,
            tx_root: v1.tx_root,
            tx_refs,
            withdrawals_ref: 0,
        }
    }
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
            DehydratedBidSubmission::Fulu(
                DehydratedBidSubmissionFuluV1 {
                    message: self.message,
                    execution_payload: self.execution_payload,
                    blobs_bundle: self.blobs_bundle,
                    execution_requests: self.execution_requests,
                    signature: self.signature,
                    tx_root: self.tx_root,
                }
                .into(),
            ),
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
            DehydratedBidSubmission::Fulu(
                DehydratedBidSubmissionFuluV1 {
                    message: self.message,
                    execution_payload: self.execution_payload,
                    blobs_bundle: self.blobs_bundle,
                    execution_requests: self.execution_requests,
                    signature: self.signature,
                    tx_root: self.tx_root,
                }
                .into(),
            ),
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

/// v2 TCP bodies: `[submission][adjustments?][merging?]`, the trailers present
/// per the header flags. Generic so no shape needs a struct per combination.
#[derive(Debug, Clone, Decode)]
pub struct WithAdjustments<S: ssz::Decode, A: ssz::Decode> {
    pub submission: S,
    pub adjustments: A,
}

#[derive(Debug, Clone, Decode)]
pub struct WithMergingData<S: ssz::Decode, M: ssz::Decode> {
    pub submission: S,
    pub merging_data: M,
}

#[derive(Debug, Clone, Decode)]
pub struct WithAdjustmentsAndMergingData<S: ssz::Decode, A: ssz::Decode, M: ssz::Decode> {
    pub submission: S,
    pub adjustments: A,
    pub merging_data: M,
}

impl TestRandom for DehydratedBidSubmissionFuluV1 {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        Self {
            message: BidTrace::random_for_test(rng),
            execution_payload: ExecutionPayload::random_for_test(rng),
            blobs_bundle: DehydratedBlobsFulu { commitments: vec![], new_items: vec![] },
            execution_requests: Arc::new(ExecutionRequests::random_for_test(rng)),
            signature: BlsSignatureBytes::random(),
            tx_root: None,
        }
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
            DehydratedBidSubmission::Fulu(
                DehydratedBidSubmissionFuluV1 {
                    message: self.message,
                    execution_payload: self.execution_payload,
                    blobs_bundle: self.blobs_bundle,
                    execution_requests: self.execution_requests,
                    signature: self.signature,
                    tx_root: self.tx_root,
                }
                .into(),
            ),
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

/// Withdrawals cache key: FxHash over each withdrawal's fields. Mirrored by
/// the builder for v2 `withdrawals_ref`.
pub fn withdrawals_key(withdrawals: &[Withdrawal]) -> u64 {
    let mut hasher = FxHasher::default();
    for w in withdrawals {
        hasher.write_u64(w.index);
        hasher.write_u64(w.validator_index);
        hasher.write(w.address.as_slice());
        hasher.write_u64(w.amount);
    }
    hasher.finish()
}

/// Blob cache key: FxHash of the 48-byte commitment. Mirrored by the builder
/// for v2 `blob_refs`.
pub fn blob_cache_key(commitment: &KzgCommitment) -> u64 {
    let mut hasher = FxHasher::default();
    hasher.write(commitment.as_slice());
    hasher.finish()
}

impl DehydratedBidSubmissionFulu {
    #[timed]
    fn can_hydrate_inner(
        &self,
        txs: &FxHashMap<u64, Transaction>,
        blobs: &FxHashMap<u64, CachedBlob>,
        withdrawals: &FxHashMap<u64, Withdrawals>,
        max_blobs_per_block: usize,
    ) -> bool {
        self.blobs_bundle.refs.len() <= max_blobs_per_block &&
            (self.withdrawals_ref == 0 || withdrawals.contains_key(&self.withdrawals_ref)) &&
            self.tx_refs.iter().all(|&r| r == NEXT_FULL_ITEM || txs.contains_key(&r)) &&
            self.execution_payload.transactions.iter().all(|tx| tx.len() >= TX_KEY_SIZE) &&
            self.blobs_bundle.refs.iter().all(|&r| r == NEXT_FULL_ITEM || blobs.contains_key(&r))
    }

    #[timed]
    fn hydrate_inner(
        mut self,
        txs: &mut FxHashMap<u64, Transaction>,
        blobs: &mut FxHashMap<u64, CachedBlob>,
        withdrawals: &mut FxHashMap<u64, Withdrawals>,
        max_blobs_per_block: usize,
    ) -> Result<HydratedData, HydrationError> {
        // avoid short-circuiting the loop to maximize cache population
        let mut last_err = Ok(());
        let mut tx_cache_hits = 0;

        if self.withdrawals_ref != 0 {
            match withdrawals.get(&self.withdrawals_ref) {
                Some(w) => self.execution_payload.withdrawals = w.clone(),
                None => {
                    last_err = Err(HydrationError::UnknownWithdrawals { key: self.withdrawals_ref })
                }
            }
        } else if !self.execution_payload.withdrawals.is_empty() {
            let w = &self.execution_payload.withdrawals;
            withdrawals.insert(withdrawals_key(w), w.clone());
        }

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
                    let hash = tx_cache_key(&tx);
                    txs.insert(hash, tx.clone());
                    trace!("Inserted tx into cache: index {}, hash {}", index, hash);
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

        // hydrate blobs

        let mut blob_cache_hits: usize = 0;
        let new_blobs = self.blobs_bundle.new_items.len();
        let mut new_keys = Vec::with_capacity(new_blobs);
        for blob_item in self.blobs_bundle.new_items {
            let key = blob_cache_key(&blob_item.commitment);
            new_keys.push(key);
            blobs.insert(key, (blob_item.commitment, blob_item.proof, blob_item.blob));
        }

        let num_blobs = self.blobs_bundle.refs.len();
        if num_blobs > max_blobs_per_block {
            last_err =
                Err(HydrationError::TooManyBlobs { blobs: num_blobs, max: max_blobs_per_block });
        }

        last_err?;

        let mut sidecar = BlobsBundle::with_capacity(num_blobs);
        let mut new_keys = new_keys.into_iter();
        for (index, &blob_ref) in self.blobs_bundle.refs.iter().enumerate() {
            let key = if blob_ref == NEXT_FULL_ITEM {
                new_keys.next().ok_or(HydrationError::MissingNewBlob { index })?
            } else {
                blob_ref
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
        withdrawals: &mut FxHashMap<u64, Withdrawals>,
    ) {
        if self.withdrawals_ref == 0 && !self.execution_payload.withdrawals.is_empty() {
            let w = &self.execution_payload.withdrawals;
            withdrawals.insert(withdrawals_key(w), w.clone());
        }
        for tx in &self.execution_payload.transactions {
            if tx.len() >= TX_KEY_SIZE {
                txs.insert(tx_cache_key(tx), tx.clone());
            }
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
    withdrawals: FxHashMap<u64, Withdrawals>,
}

impl Cache {
    fn new() -> Self {
        Self {
            transactions: FxHashMap::with_capacity_and_hasher(10_000, Default::default()),
            blobs_fulu: FxHashMap::with_capacity_and_hasher(1_000, Default::default()),
            withdrawals: FxHashMap::with_capacity_and_hasher(4, Default::default()),
        }
    }

    fn clear(&mut self) {
        self.transactions.clear();
        self.blobs_fulu.clear();
        self.withdrawals.clear();
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
    withdrawals: FxHashMap<u64, Withdrawals>,
}

impl SimHydrationCache {
    pub fn new() -> Self {
        Self {
            transactions: FxHashMap::with_capacity_and_hasher(10_000, Default::default()),
            blobs_fulu: FxHashMap::with_capacity_and_hasher(1_000, Default::default()),
            withdrawals: FxHashMap::with_capacity_and_hasher(4, Default::default()),
        }
    }

    pub fn can_hydrate(
        &self,
        submission: &DehydratedBidSubmission,
        max_blobs_per_block: usize,
    ) -> bool {
        match submission {
            DehydratedBidSubmission::Fulu(s) => s.can_hydrate_inner(
                &self.transactions,
                &self.blobs_fulu,
                &self.withdrawals,
                max_blobs_per_block,
            ),
        }
    }

    pub fn hydrate(
        &mut self,
        submission: DehydratedBidSubmission,
        max_blobs_per_block: usize,
    ) -> Result<HydratedData, HydrationError> {
        match submission {
            DehydratedBidSubmission::Fulu(s) => s.hydrate_inner(
                &mut self.transactions,
                &mut self.blobs_fulu,
                &mut self.withdrawals,
                max_blobs_per_block,
            ),
        }
    }

    /// Inserts the submission's full transactions and new blobs into the cache
    /// without building the hydrated payload, so a submission the sim tile will
    /// not hydrate still lets later submissions resolve their references.
    pub fn feed(&mut self, submission: &DehydratedBidSubmission) {
        match submission {
            DehydratedBidSubmission::Fulu(s) => {
                s.feed_inner(&mut self.transactions, &mut self.blobs_fulu, &mut self.withdrawals);
            }
        }
    }

    pub fn clear(&mut self) {
        self.transactions.clear();
        self.blobs_fulu.clear();
        self.withdrawals.clear();
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
                let empty = Cache::default();
                let c = self.caches.get(&s.message.builder_pubkey).unwrap_or(&empty);
                s.can_hydrate_inner(
                    &c.transactions,
                    &c.blobs_fulu,
                    &c.withdrawals,
                    max_blobs_per_block,
                )
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
                s.hydrate_inner(
                    &mut entry.transactions,
                    &mut entry.blobs_fulu,
                    &mut entry.withdrawals,
                    max_blobs_per_block,
                )
            }
        }
    }

    /// Inserts the submission's full transactions and new blobs into the
    /// builder's cache without building the hydrated payload.
    pub fn feed(&mut self, submission: &DehydratedBidSubmission) {
        match submission {
            DehydratedBidSubmission::Fulu(s) => {
                let entry = self.caches.entry(s.message.builder_pubkey).or_default();
                s.feed_inner(
                    &mut entry.transactions,
                    &mut entry.blobs_fulu,
                    &mut entry.withdrawals,
                );
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

    #[error("blob ref {index} expects a new blob but none remain")]
    MissingNewBlob { index: usize },

    #[error("too many txs")]
    TooManyTxs,

    #[error("unknown withdrawals: key {key}")]
    UnknownWithdrawals { key: u64 },
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
    let mut v1 = DehydratedBidSubmissionFuluV1::random_for_test(&mut rand::rng());
    v1.execution_payload.transactions = Transactions::new(txs).expect("tx list within limits");
    DehydratedBidSubmission::Fulu(v1.into())
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
}
