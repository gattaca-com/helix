use std::hash::Hasher;

use alloy_primitives::{Address, B256};
use alloy_rlp::Header;
use bytes::Bytes;
use lh_types::{ForkName, ForkVersionDecode};
use rustc_hash::FxHasher;
use serde::{Deserialize, Serialize};
use silver_ssz::ssz_view::ExecutionPayloadView;
use ssz::{Decode, DecodeError, Encode};
use ssz_derive::{Decode, Encode};

use super::{
    Cache, DehydratedBidSubmission, DehydratedBlobsFulu, DehydratedBlobsFuluV2, HydratedData,
    HydrationError, NEXT_FULL_ITEM, TX_KEY_SIZE, blob_cache_key, cache_new_blob, cache_withdrawals,
    refs_resolvable, tx_cache_key,
};
use crate::{
    BidTrace, BlockMergingData, BlockValidationError, BlsPublicKeyBytes, BlsSignatureBytes,
    ExecutionPayloadGloas, ExecutionRequestsGloas, GloasBidSubmission, GloasBidTrace,
    SignedBidSubmission, Transaction, Withdrawals,
    gloas::{
        SUBMISSION_FIXED, payload_layout_ok, transaction_slices, validate_gloas_payload,
        withdrawals_root_of_bytes,
    },
};

const TRANSACTIONS_OFFSET_AT: usize = 504;
const WITHDRAWALS_OFFSET_AT: usize = 508;
const BLOCK_ACCESS_LIST_OFFSET_AT: usize = 528;

#[derive(Debug, Clone)]
pub struct DehydratedBidSubmissionGloas(Parts);

#[derive(Debug, Clone, Decode)]
struct Parts {
    message: GloasBidTrace,
    execution_payload: Vec<u8>,
    blobs_bundle: DehydratedBlobsFuluV2,
    execution_requests: Vec<u8>,
    signature: BlsSignatureBytes,
    tx_root: Option<B256>,
    tx_refs: Vec<u64>,
    withdrawals_ref: u64,
    /// Block order of the access list entries as cache keys, [`NEXT_FULL_ITEM`]
    /// meaning the next entry of the inline list.
    bal_refs: Vec<u64>,
}

/// Access list entry cache key: FxHash of the entry's RLP bytes. Mirrored by
/// the builder for `bal_refs`.
pub fn bal_entry_key(entry: &[u8]) -> u64 {
    let mut hasher = FxHasher::default();
    hasher.write(entry);
    hasher.finish()
}

fn split_rlp_item(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut rest = bytes;
    let header = Header::decode(&mut rest).ok()?;
    bytes.split_at_checked(bytes.len() - rest.len() + header.payload_length)
}

/// Entries of an RLP-encoded access list, `None` when the encoding is malformed.
fn bal_entries(list: &[u8]) -> Option<impl Iterator<Item = &[u8]>> {
    let mut rest = list;
    let header = Header::decode(&mut rest).ok()?;
    if !header.list || rest.len() != header.payload_length {
        return None;
    }
    let mut check = rest;
    while !check.is_empty() {
        check = split_rlp_item(check)?.1;
    }
    Some(std::iter::from_fn(move || {
        let (entry, tail) = split_rlp_item(rest)?;
        rest = tail;
        Some(entry)
    }))
}

fn assemble_payload(head: &[u8], tx_list: &[u8], withdrawals: &[u8], bal: &[u8]) -> Vec<u8> {
    let mut payload =
        Vec::with_capacity(head.len() + tx_list.len() + withdrawals.len() + bal.len());
    payload.extend_from_slice(head);
    let transactions_offset = payload.len() as u32;
    payload.extend_from_slice(tx_list);
    let withdrawals_offset = payload.len() as u32;
    payload.extend_from_slice(withdrawals);
    let block_access_list_offset = payload.len() as u32;
    payload.extend_from_slice(bal);
    for (at, offset) in [
        (TRANSACTIONS_OFFSET_AT, transactions_offset),
        (WITHDRAWALS_OFFSET_AT, withdrawals_offset),
        (BLOCK_ACCESS_LIST_OFFSET_AT, block_access_list_offset),
    ] {
        payload[at..at + 4].copy_from_slice(&offset.to_le_bytes());
    }
    payload
}

/// v1 wire shape (HTTP JSON/SSZ): a cached tx is its 8-byte key inline in
/// `transactions`, a cached blob its commitment; withdrawals and the access
/// list are inline. Converted into [`DehydratedBidSubmissionGloas`] on decode.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct DehydratedBidSubmissionGloasV1 {
    message: GloasBidTrace,
    execution_payload: ExecutionPayloadGloas,
    blobs_bundle: DehydratedBlobsFulu,
    execution_requests: ExecutionRequestsGloas,
    signature: BlsSignatureBytes,
    tx_root: Option<B256>,
}

impl ForkVersionDecode for DehydratedBidSubmissionGloasV1 {
    fn from_ssz_bytes_by_fork(bytes: &[u8], fork: ForkName) -> Result<Self, DecodeError> {
        match fork {
            ForkName::Gloas => Self::from_ssz_bytes(bytes),
            _ => Err(DecodeError::NoMatchingVariant),
        }
    }
}

impl From<DehydratedBidSubmissionGloasV1> for DehydratedBidSubmissionGloas {
    fn from(v1: DehydratedBidSubmissionGloasV1) -> Self {
        let payload = v1.execution_payload.as_ssz_bytes();
        let transactions_offset = ExecutionPayloadView::transactions_offset(&payload) as usize;
        let withdrawals_offset = ExecutionPayloadView::withdrawals_offset(&payload) as usize;
        let bal_offset = ExecutionPayloadView::block_access_list_offset(&payload) as usize;
        let transactions = ExecutionPayloadView::transactions(&payload);
        let table =
            4 * transaction_slices(transactions).filter(|tx| tx.len() != size_of::<u64>()).count();
        let txs = transaction_slices(transactions);
        let mut tx_refs = Vec::with_capacity(txs.len());
        let mut tx_list = Vec::with_capacity(table);
        let mut tx_body = Vec::new();
        for tx in txs {
            match <[u8; 8]>::try_from(tx) {
                Ok(key) => tx_refs.push(u64::from_le_bytes(key)),
                Err(_) => {
                    tx_refs.push(NEXT_FULL_ITEM);
                    tx_list.extend_from_slice(&((table + tx_body.len()) as u32).to_le_bytes());
                    tx_body.extend_from_slice(tx);
                }
            }
        }
        tx_list.extend_from_slice(&tx_body);
        let bal = &payload[bal_offset..];
        Self(Parts {
            message: v1.message,
            execution_payload: assemble_payload(
                &payload[..transactions_offset],
                &tx_list,
                &payload[withdrawals_offset..bal_offset],
                bal,
            ),
            blobs_bundle: DehydratedBlobsFuluV2 {
                refs: v1.blobs_bundle.commitments.iter().map(blob_cache_key).collect(),
                new_items: v1.blobs_bundle.new_items,
            },
            execution_requests: v1.execution_requests.as_ssz_bytes(),
            signature: v1.signature,
            tx_root: v1.tx_root,
            tx_refs,
            withdrawals_ref: 0,
            bal_refs: vec![NEXT_FULL_ITEM; bal_entries(bal).map_or(0, Iterator::count)],
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct DehydratedBidSubmissionGloasV1WithMergingData {
    message: GloasBidTrace,
    execution_payload: ExecutionPayloadGloas,
    blobs_bundle: DehydratedBlobsFulu,
    execution_requests: ExecutionRequestsGloas,
    signature: BlsSignatureBytes,
    tx_root: Option<B256>,
    merging_data: BlockMergingData,
}

impl DehydratedBidSubmissionGloasV1WithMergingData {
    pub fn split(self) -> (DehydratedBidSubmission, BlockMergingData) {
        let v1 = DehydratedBidSubmissionGloasV1 {
            message: self.message,
            execution_payload: self.execution_payload,
            blobs_bundle: self.blobs_bundle,
            execution_requests: self.execution_requests,
            signature: self.signature,
            tx_root: self.tx_root,
        };
        (DehydratedBidSubmission::Gloas(v1.into()), self.merging_data)
    }
}

impl ForkVersionDecode for DehydratedBidSubmissionGloasV1WithMergingData {
    fn from_ssz_bytes_by_fork(bytes: &[u8], fork: ForkName) -> Result<Self, DecodeError> {
        match fork {
            ForkName::Gloas => Self::from_ssz_bytes(bytes),
            _ => Err(DecodeError::NoMatchingVariant),
        }
    }
}

impl Decode for DehydratedBidSubmissionGloas {
    fn is_ssz_fixed_len() -> bool {
        false
    }

    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let parts = Parts::from_ssz_bytes(bytes)?;
        if !payload_layout_ok(&parts.execution_payload) {
            return Err(DecodeError::BytesInvalid("execution payload layout".to_string()));
        }
        Ok(Self(parts))
    }
}

impl DehydratedBidSubmissionGloas {
    fn payload(&self) -> &[u8] {
        &self.0.execution_payload
    }

    pub fn bid_trace(&self) -> &BidTrace {
        &self.0.message.trace
    }

    pub fn parent_block_root(&self) -> B256 {
        self.0.message.parent_block_root
    }

    pub fn fee_recipient(&self) -> Address {
        Address::from(*ExecutionPayloadView::fee_recipient(self.payload()))
    }

    pub fn timestamp(&self) -> u64 {
        ExecutionPayloadView::timestamp(self.payload())
    }

    pub fn block_number(&self) -> u64 {
        ExecutionPayloadView::block_number(self.payload())
    }

    pub fn prev_randao(&self) -> &B256 {
        <&B256>::from(ExecutionPayloadView::prev_randao(self.payload()))
    }

    pub fn num_txs(&self) -> usize {
        self.0.tx_refs.len()
    }

    pub fn withdrawals_ref(&self) -> u64 {
        self.0.withdrawals_ref
    }

    pub fn withdrawal_root(&self) -> Option<B256> {
        (self.0.withdrawals_ref == 0).then(|| withdrawals_root_of_bytes(self.withdrawals()))
    }

    pub fn validate(&self) -> Result<(), BlockValidationError> {
        validate_gloas_payload(self.bid_trace(), self.payload())
    }

    pub fn set_builder_pubkey_for_test(&mut self, pubkey: BlsPublicKeyBytes) {
        self.0.message.trace.builder_pubkey = pubkey;
    }

    fn withdrawals(&self) -> &[u8] {
        let payload = self.payload();
        &payload[ExecutionPayloadView::withdrawals_offset(payload) as usize..
            ExecutionPayloadView::block_access_list_offset(payload) as usize]
    }

    fn full_txs(&self) -> impl ExactSizeIterator<Item = &[u8]> {
        transaction_slices(ExecutionPayloadView::transactions(self.payload()))
    }

    fn block_access_list(&self) -> &[u8] {
        let payload = self.payload();
        &payload[ExecutionPayloadView::block_access_list_offset(payload) as usize..]
    }

    pub(super) fn can_hydrate(&self, cache: &Cache, max_blobs_per_block: usize) -> bool {
        let Some(inline) = bal_entries(self.block_access_list()) else {
            return false;
        };
        let bal_refs = &self.0.bal_refs;
        refs_resolvable(
            &self.0.tx_refs,
            self.full_txs().map(|tx| tx.len()),
            &self.0.blobs_bundle,
            self.0.withdrawals_ref,
            cache,
            max_blobs_per_block,
        ) && bal_refs.iter().filter(|&&r| r == NEXT_FULL_ITEM).count() <= inline.count() &&
            bal_refs.iter().all(|&r| r == NEXT_FULL_ITEM || cache.bal_entries.contains_key(&r))
    }

    pub(super) fn feed(&self, cache: &mut Cache) {
        if let Some(entries) = bal_entries(self.block_access_list()) {
            for entry in entries {
                cache.bal_entries.insert(bal_entry_key(entry), entry.to_vec());
            }
        }
        if self.0.withdrawals_ref == 0 &&
            let Ok(withdrawals) = Withdrawals::from_ssz_bytes(self.withdrawals())
        {
            cache_withdrawals(&mut cache.withdrawals, &withdrawals);
        }
        for tx in self.full_txs() {
            if tx.len() >= TX_KEY_SIZE {
                let tx = Transaction(Bytes::copy_from_slice(tx).into());
                cache.transactions.insert(tx_cache_key(&tx), tx);
            }
        }
        for item in &self.0.blobs_bundle.new_items {
            cache_new_blob(&mut cache.blobs_fulu, item.clone());
        }
    }

    pub(super) fn hydrate(
        self,
        cache: &mut Cache,
        max_blobs_per_block: usize,
    ) -> Result<HydratedData, HydrationError> {
        let Parts {
            message,
            execution_payload,
            blobs_bundle,
            execution_requests,
            signature,
            tx_root,
            tx_refs,
            withdrawals_ref,
            bal_refs,
        } = self.0;
        let dehydrated = execution_payload.as_slice();
        let withdrawals_offset = ExecutionPayloadView::withdrawals_offset(dehydrated) as usize;
        let block_access_list_offset =
            ExecutionPayloadView::block_access_list_offset(dehydrated) as usize;
        let mut last_err = Ok(());

        let withdrawals = if withdrawals_ref != 0 {
            match cache.withdrawals.get(&withdrawals_ref) {
                Some((withdrawals, _)) => withdrawals.as_ssz_bytes(),
                None => {
                    last_err = Err(HydrationError::UnknownWithdrawals { key: withdrawals_ref });
                    Vec::new()
                }
            }
        } else {
            let inline = &dehydrated[withdrawals_offset..block_access_list_offset];
            if let Ok(withdrawals) = Withdrawals::from_ssz_bytes(inline) {
                cache_withdrawals(&mut cache.withdrawals, &withdrawals);
            }
            inline.to_vec()
        };

        let mut full = transaction_slices(ExecutionPayloadView::transactions(dehydrated));
        let table = 4 * tx_refs.len();
        let mut tx_list = Vec::with_capacity(table);
        let mut tx_body = Vec::new();
        let mut tx_cache_hits = 0;
        for (index, &tx_ref) in tx_refs.iter().enumerate() {
            let tx: &[u8] = if tx_ref == NEXT_FULL_ITEM {
                let Some(tx) = full.next() else {
                    last_err = Err(HydrationError::MissingFullTx { index });
                    continue;
                };
                if tx.len() < TX_KEY_SIZE {
                    last_err = Err(HydrationError::InvalidTxLength { length: tx.len(), index });
                } else {
                    let cached = Transaction(Bytes::copy_from_slice(tx).into());
                    cache.transactions.insert(tx_cache_key(&cached), cached);
                }
                tx
            } else if let Some(cached) = cache.transactions.get(&tx_ref) {
                tx_cache_hits += 1;
                cached
            } else {
                last_err = Err(HydrationError::UnknownTxHash { index, hash: tx_ref });
                continue;
            };
            tx_list.extend_from_slice(&((table + tx_body.len()) as u32).to_le_bytes());
            tx_body.extend_from_slice(tx);
        }
        tx_list.extend_from_slice(&tx_body);

        let mut inline = bal_entries(&dehydrated[block_access_list_offset..]);
        if inline.is_none() {
            last_err = Err(HydrationError::InvalidBlockAccessList);
        }
        let mut bal_body = Vec::new();
        let mut bal_cache_hits = 0;
        for (index, &bal_ref) in bal_refs.iter().enumerate() {
            let entry: &[u8] = if bal_ref == NEXT_FULL_ITEM {
                let Some(entry) = inline.as_mut().and_then(Iterator::next) else {
                    last_err = Err(HydrationError::MissingFullBalEntry { index });
                    continue;
                };
                cache.bal_entries.insert(bal_entry_key(entry), entry.to_vec());
                entry
            } else if let Some(cached) = cache.bal_entries.get(&bal_ref) {
                bal_cache_hits += 1;
                cached
            } else {
                last_err = Err(HydrationError::UnknownBalEntry { index, key: bal_ref });
                continue;
            };
            bal_body.extend_from_slice(entry);
        }
        let mut block_access_list = Vec::with_capacity(bal_body.len() + 9);
        Header { list: true, payload_length: bal_body.len() }.encode(&mut block_access_list);
        block_access_list.extend_from_slice(&bal_body);

        let mut new_keys = Vec::with_capacity(blobs_bundle.new_items.len());
        for (index, item) in blobs_bundle.new_items.into_iter().enumerate() {
            let proofs = item.proof.len();
            match cache_new_blob(&mut cache.blobs_fulu, item) {
                Some(key) => new_keys.push(key),
                None => last_err = Err(HydrationError::InvalidBlobProofs { index, proofs }),
            }
        }
        let num_blobs = blobs_bundle.refs.len();
        if num_blobs > max_blobs_per_block {
            last_err =
                Err(HydrationError::TooManyBlobs { blobs: num_blobs, max: max_blobs_per_block });
        }
        last_err?;

        let mut commitments = Vec::with_capacity(num_blobs * 48);
        let mut proofs = Vec::new();
        let mut blobs = Vec::new();
        let mut new_keys = new_keys.into_iter();
        let mut blob_cache_hits = 0;
        for (index, &blob_ref) in blobs_bundle.refs.iter().enumerate() {
            let key = if blob_ref == NEXT_FULL_ITEM {
                new_keys.next().ok_or(HydrationError::MissingNewBlob { index })?
            } else {
                blob_cache_hits += 1;
                blob_ref
            };
            let Some((commitment, cell_proofs, blob)) = cache.blobs_fulu.get(&key) else {
                return Err(HydrationError::UnknownBlobHashFulu { key, index });
            };
            commitments.extend_from_slice(commitment.as_slice());
            for proof in cell_proofs {
                proofs.extend_from_slice(proof.as_slice());
            }
            blobs.extend_from_slice(blob.as_slice());
        }
        let proofs_offset = 12 + commitments.len();
        let blobs_offset = proofs_offset + proofs.len();
        let mut bundle = Vec::with_capacity(blobs_offset + blobs.len());
        bundle.extend_from_slice(&12u32.to_le_bytes());
        bundle.extend_from_slice(&(proofs_offset as u32).to_le_bytes());
        bundle.extend_from_slice(&(blobs_offset as u32).to_le_bytes());
        bundle.extend_from_slice(&commitments);
        bundle.extend_from_slice(&proofs);
        bundle.extend_from_slice(&blobs);

        let payload = assemble_payload(
            &dehydrated[..ExecutionPayloadView::transactions_offset(dehydrated) as usize],
            &tx_list,
            &withdrawals,
            &block_access_list,
        );

        let blobs_offset = SUBMISSION_FIXED + payload.len();
        let requests_offset = blobs_offset + bundle.len();
        let mut submission = Vec::with_capacity(requests_offset + execution_requests.len());
        message.ssz_append(&mut submission);
        submission.extend_from_slice(&(SUBMISSION_FIXED as u32).to_le_bytes());
        submission.extend_from_slice(&(blobs_offset as u32).to_le_bytes());
        submission.extend_from_slice(&(requests_offset as u32).to_le_bytes());
        submission.extend_from_slice(signature.as_slice());
        submission.extend_from_slice(&payload);
        submission.extend_from_slice(&bundle);
        submission.extend_from_slice(&execution_requests);
        let submission = GloasBidSubmission::from_ssz_bytes(Bytes::from(submission))
            .map_err(HydrationError::Hydrated)?;

        Ok(HydratedData {
            submission: SignedBidSubmission::Gloas(submission),
            tx_cache_hits,
            blob_cache_hits,
            bal_cache_hits,
            tx_root,
        })
    }
}
