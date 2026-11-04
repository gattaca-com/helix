use alloy_primitives::{Address, B256};
use bytes::Bytes;
use rand::Rng;
use silver_crypto_simd::{
    bls::{G1Point, G2Point, PairingProduct, uncompress_in_g1, uncompress_in_g2},
    kzg::CELLS_PER_EXT_BLOB,
};
use silver_ssz::{
    merkle::{hash_concat, merkleize, uint64_chunk},
    ssz_hash_gloas::ExecutionRequestsView,
    ssz_view::{
        BYTES_PER_BLOB, BYTES_PER_KZG_COMMITMENT, BYTES_PER_KZG_PROOF, BeaconBlockBodyGloasView,
        EXECUTION_PAYLOAD_FIXED_GLOAS, ExecutionPayloadBidView, ExecutionPayloadEnvelopeView,
        ExecutionPayloadView, MAX_BYTES_PER_TRANSACTION, MAX_EXTRA_DATA_BYTES,
        MAX_TRANSACTIONS_PER_PAYLOAD, MAX_WITHDRAWALS_PER_PAYLOAD, SignedBeaconBlockView,
        WithdrawalView,
    },
};
use ssz::{Decode, DecodeError};
use tree_hash::TreeHash;

use crate::{
    BidTrace, BlockValidationError, GloasBidTrace, SszError, error::SigError,
    execution_payload::withdrawals_root_of,
};

const GLOAS_BID_TRACE_SIZE: usize = 268;
const SUBMISSION_FIXED: usize = GLOAS_BID_TRACE_SIZE + 3 * 4 + 96;
const WITHDRAWAL_SIZE: usize = 44;
const BID_FIXED: usize = 224;
const ENVELOPE_FIXED: usize = 80;
const SIGNED_FIXED: usize = 100;
const BLS_DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

fn u32_at(buf: &[u8], at: usize) -> usize {
    u32::from_le_bytes(buf[at..at + 4].try_into().expect("4 bytes")) as usize
}

fn invalid(what: &str) -> DecodeError {
    DecodeError::BytesInvalid(what.to_string())
}

#[derive(Clone, Debug)]
pub struct GloasBidSubmission {
    pub message: GloasBidTrace,
    bytes: Bytes,
    blobs_offset: usize,
    requests_offset: usize,
}

impl GloasBidSubmission {
    pub fn from_ssz_bytes(bytes: Bytes) -> Result<Self, DecodeError> {
        if bytes.len() < SUBMISSION_FIXED {
            return Err(DecodeError::InvalidByteLength {
                len: bytes.len(),
                expected: SUBMISSION_FIXED,
            });
        }
        let message = GloasBidTrace::from_ssz_bytes(&bytes[..GLOAS_BID_TRACE_SIZE])?;
        let payload_offset = u32_at(&bytes, GLOAS_BID_TRACE_SIZE);
        let blobs_offset = u32_at(&bytes, GLOAS_BID_TRACE_SIZE + 4);
        let requests_offset = u32_at(&bytes, GLOAS_BID_TRACE_SIZE + 8);
        if payload_offset != SUBMISSION_FIXED ||
            blobs_offset < payload_offset ||
            requests_offset < blobs_offset ||
            requests_offset > bytes.len()
        {
            return Err(invalid("submission offsets"));
        }
        let submission = Self { message, bytes, blobs_offset, requests_offset };
        if !payload_layout_ok(submission.execution_payload()) {
            return Err(invalid("execution payload layout"));
        }
        if !blobs_bundle_layout_ok(submission.blobs_bundle()) {
            return Err(invalid("blobs bundle layout"));
        }
        if !ExecutionRequestsView::check_canonical(submission.execution_requests()) {
            return Err(invalid("execution requests layout"));
        }
        Ok(submission)
    }

    pub fn as_bytes(&self) -> &Bytes {
        &self.bytes
    }

    pub fn execution_payload(&self) -> &[u8] {
        &self.bytes[SUBMISSION_FIXED..self.blobs_offset]
    }

    pub fn blobs_bundle(&self) -> &[u8] {
        &self.bytes[self.blobs_offset..self.requests_offset]
    }

    pub fn block_access_list(&self) -> &[u8] {
        let payload = self.execution_payload();
        &payload[ExecutionPayloadView::block_access_list_offset(payload) as usize..]
    }

    fn execution_requests(&self) -> &[u8] {
        &self.bytes[self.requests_offset..]
    }

    pub fn signature(&self) -> &[u8] {
        &self.bytes[GLOAS_BID_TRACE_SIZE + 12..SUBMISSION_FIXED]
    }

    pub fn bid_trace(&self) -> &BidTrace {
        &self.message.trace
    }

    pub fn verify_signature(&self, builder_domain: B256) -> Result<(), SigError> {
        bls_verify(
            self.message.trace.builder_pubkey.as_slice(),
            self.signature(),
            signing_root(self.message.tree_hash_root(), builder_domain),
        )
    }

    pub fn validate_lengths(&self, max_blobs_per_block: usize) -> Result<(), BlockValidationError> {
        let blobs = self.blobs().len() / BYTES_PER_BLOB;
        if blobs > max_blobs_per_block {
            return Err(BlockValidationError::SszError(SszError::InvalidByteCount {
                given: blobs,
                expected: max_blobs_per_block,
            }));
        }
        ExecutionRequestsView::check_counts(self.execution_requests()).map_err(|err| {
            BlockValidationError::SszError(SszError::InvalidByteCount {
                given: err.count,
                expected: err.max,
            })
        })
    }

    pub fn validate(&self) -> Result<(), BlockValidationError> {
        let trace = &self.message.trace;
        let payload = self.execution_payload();
        let parent_hash = B256::from(*ExecutionPayloadView::parent_hash(payload));
        let block_hash = B256::from(*ExecutionPayloadView::block_hash(payload));
        if trace.parent_hash != parent_hash {
            return Err(BlockValidationError::ParentHashMismatch {
                message: trace.parent_hash,
                payload: parent_hash,
            });
        }
        if trace.block_hash != block_hash {
            return Err(BlockValidationError::BlockHashMismatch {
                message: trace.block_hash,
                payload: block_hash,
            });
        }
        if trace.gas_limit != ExecutionPayloadView::gas_limit(payload) {
            return Err(BlockValidationError::GasLimitMismatch {
                message: trace.gas_limit,
                payload: ExecutionPayloadView::gas_limit(payload),
            });
        }
        if trace.gas_used != ExecutionPayloadView::gas_used(payload) {
            return Err(BlockValidationError::GasUsedMismatch {
                message: trace.gas_used,
                payload: ExecutionPayloadView::gas_used(payload),
            });
        }
        if trace.slot != ExecutionPayloadView::slot_number(payload) {
            return Err(BlockValidationError::SlotMismatch {
                got: ExecutionPayloadView::slot_number(payload),
                expected: trace.slot,
            });
        }
        if trace.value.is_zero() {
            return Err(BlockValidationError::ZeroValueBlock);
        }
        Ok(())
    }

    pub fn prev_randao(&self) -> &B256 {
        <&B256>::from(ExecutionPayloadView::prev_randao(self.execution_payload()))
    }

    pub fn timestamp(&self) -> u64 {
        ExecutionPayloadView::timestamp(self.execution_payload())
    }

    pub fn block_number(&self) -> u64 {
        ExecutionPayloadView::block_number(self.execution_payload())
    }

    pub fn fee_recipient(&self) -> Address {
        Address::from(*ExecutionPayloadView::fee_recipient(self.execution_payload()))
    }

    pub fn num_txs(&self) -> usize {
        let txs = ExecutionPayloadView::transactions(self.execution_payload());
        if txs.is_empty() { 0 } else { u32_at(txs, 0) / 4 }
    }

    pub fn withdrawals_root(&self) -> B256 {
        let payload = self.execution_payload();
        let withdrawals = &payload[ExecutionPayloadView::withdrawals_offset(payload) as usize..
            ExecutionPayloadView::block_access_list_offset(payload) as usize];
        // `from_ssz_bytes` validates complete 44-byte entries and at most 16 withdrawals.
        withdrawals_root_of(withdrawals.chunks_exact(WITHDRAWAL_SIZE).map(|w| {
            let w = w.try_into().expect("one withdrawal");
            let address = WithdrawalView::address(w);
            (
                WithdrawalView::index(w),
                WithdrawalView::validator_index(w),
                address,
                WithdrawalView::amount(w),
            )
        }))
    }

    fn bundle_section(&self, i: usize) -> &[u8] {
        let bundle = self.blobs_bundle();
        let start = u32_at(bundle, 4 * i);
        let end = if i < 2 { u32_at(bundle, 4 * (i + 1)) } else { bundle.len() };
        &bundle[start..end]
    }

    fn commitments(&self) -> &[u8] {
        self.bundle_section(0)
    }

    fn proofs(&self) -> &[u8] {
        self.bundle_section(1)
    }

    fn blobs(&self) -> &[u8] {
        self.bundle_section(2)
    }

    pub fn unsigned_bid(&self, fee_recipient: Address, execution_payment: u64) -> Vec<u8> {
        let payload = self.execution_payload();
        let commitments = self.commitments();
        let mut bid = Vec::with_capacity(BID_FIXED + commitments.len());
        bid.extend_from_slice(ExecutionPayloadView::parent_hash(payload));
        bid.extend_from_slice(self.message.parent_block_root.as_slice());
        bid.extend_from_slice(ExecutionPayloadView::block_hash(payload));
        bid.extend_from_slice(ExecutionPayloadView::prev_randao(payload));
        bid.extend_from_slice(fee_recipient.as_slice());
        bid.extend_from_slice(&ExecutionPayloadView::gas_limit(payload).to_le_bytes());
        bid.extend_from_slice(&0u64.to_le_bytes());
        bid.extend_from_slice(&ExecutionPayloadView::slot_number(payload).to_le_bytes());
        bid.extend_from_slice(&0u64.to_le_bytes());
        bid.extend_from_slice(&execution_payment.to_le_bytes());
        bid.extend_from_slice(&(BID_FIXED as u32).to_le_bytes());
        bid.extend_from_slice(&ExecutionRequestsView::hash_tree_root(self.execution_requests()));
        bid.extend_from_slice(commitments);
        bid
    }

    pub fn envelope(
        &self,
        builder_index: u64,
        beacon_block_root: B256,
        parent_beacon_block_root: B256,
    ) -> Vec<u8> {
        let payload = self.execution_payload();
        let requests = self.execution_requests();
        let mut envelope = Vec::with_capacity(ENVELOPE_FIXED + payload.len() + requests.len());
        envelope.extend_from_slice(&(ENVELOPE_FIXED as u32).to_le_bytes());
        envelope.extend_from_slice(&((ENVELOPE_FIXED + payload.len()) as u32).to_le_bytes());
        envelope.extend_from_slice(&builder_index.to_le_bytes());
        envelope.extend_from_slice(beacon_block_root.as_slice());
        envelope.extend_from_slice(parent_beacon_block_root.as_slice());
        envelope.extend_from_slice(payload);
        envelope.extend_from_slice(requests);
        envelope
    }

    pub fn envelope_contents(&self, signed_envelope: &[u8]) -> Vec<u8> {
        let (proofs, blobs) = (self.proofs(), self.blobs());
        let proofs_offset = 12 + signed_envelope.len();
        let blobs_offset = proofs_offset + proofs.len();
        let mut contents = Vec::with_capacity(blobs_offset + blobs.len());
        contents.extend_from_slice(&12u32.to_le_bytes());
        contents.extend_from_slice(&(proofs_offset as u32).to_le_bytes());
        contents.extend_from_slice(&(blobs_offset as u32).to_le_bytes());
        contents.extend_from_slice(signed_envelope);
        contents.extend_from_slice(proofs);
        contents.extend_from_slice(blobs);
        contents
    }
}

pub fn bls_verify(pubkey: &[u8], signature: &[u8], root: B256) -> Result<(), SigError> {
    let signature = decode_signature(signature)?;
    bls_verify_point(&decode_pubkey(pubkey)?, &signature, root)
}

/// Verifies a signing root with an admitted G1 key and a subgroup-checked
/// G2 signature. The key must come from [`decode_pubkey`] or a `Some` entry
/// returned by [`decode_pubkeys`].
pub fn bls_verify_point(pubkey: &G1Point, signature: &G2Point, root: B256) -> Result<(), SigError> {
    let hash = G2Point::hash_to_curve(root.as_slice(), BLS_DST);
    PairingProduct::two_pairs_are_one((&G1Point::negative_generator(), signature), (pubkey, &hash))
        .then_some(())
        .ok_or(SigError::InvalidBlsSignature)
}

pub fn decode_signature(signature: &[u8]) -> Result<G2Point, SigError> {
    let signature = signature.try_into().map_err(|_| SigError::InvalidBlsSignatureBytes)?;
    let mut sig = [None];
    uncompress_in_g2(&[signature], &mut sig);
    sig[0].ok_or(SigError::InvalidBlsSignatureBytes)
}

pub fn decode_pubkey(pubkey: &[u8]) -> Result<G1Point, SigError> {
    let pubkey = pubkey.try_into().map_err(|_| SigError::InvalidBlsPubkeyBytes)?;
    let mut pk = [None];
    decode_pubkeys(&[pubkey], &mut pk);
    pk[0].ok_or(SigError::InvalidBlsPubkeyBytes)
}

/// Decodes each key as a subgroup-checked G1 point, rejecting infinity.
/// `out` must have one entry per input key; rejected keys are written as `None`.
pub fn decode_pubkeys(pubkeys: &[[u8; 48]], out: &mut [Option<G1Point>]) {
    uncompress_in_g1(pubkeys, out);
    for key in out {
        // An infinity key and signature make both pairings the identity for every message.
        if key.is_some_and(|key| key.is_infinity()) {
            *key = None;
        }
    }
}

/// Verdicts match [`bls_verify`], except for false acceptance with probability
/// at most 2⁻⁶³ per batch for fixed inputs and independent uniform RNG outputs.
/// This assumes the CSPRNG remains unpredictable to submitters.
///
/// Two or more admitted items (both points decode and the key is finite) check
/// `Π e(rᵢ·pkᵢ, H(mᵢ)) · e(−G1, Σ rᵢ·sigᵢ) == 1`.
/// Weights are 64-bit CSPRNG outputs, with zero replaced by one.
/// Both decoders check subgroup membership before weighting; checking only
/// the weighted sum can hide small-order signature components. A failed
/// batch check falls back to individual verification.
#[derive(Default)]
pub struct BlsBatch {
    pubkeys: Vec<[u8; 48]>,
    signatures: Vec<[u8; 96]>,
    roots: Vec<B256>,
    keys: Vec<Option<G1Point>>,
    sigs: Vec<Option<G2Point>>,
    included_keys: Vec<G1Point>,
    included_sigs: Vec<G2Point>,
    included_hashes: Vec<G2Point>,
    weights: Vec<u8>,
    pairing: PairingProduct,
    verdicts: Vec<bool>,
}

impl BlsBatch {
    pub fn clear(&mut self) {
        self.pubkeys.clear();
        self.signatures.clear();
        self.roots.clear();
    }

    pub fn push(&mut self, pubkey: &[u8; 48], signature: &[u8; 96], root: B256) {
        self.pubkeys.push(*pubkey);
        self.signatures.push(*signature);
        self.roots.push(root);
    }

    /// Verdicts from the last [`Self::verify`], in the order items were pushed.
    pub fn verdicts(&self) -> &[bool] {
        &self.verdicts
    }

    /// Returns true if a multi-item check was rejected. Results are in [`Self::verdicts`].
    pub fn verify(&mut self) -> bool {
        self.keys.resize(self.roots.len(), None);
        self.sigs.resize(self.roots.len(), None);
        uncompress_in_g1(&self.pubkeys, &mut self.keys);
        uncompress_in_g2(&self.signatures, &mut self.sigs);
        self.included_keys.clear();
        self.included_sigs.clear();
        self.included_hashes.clear();
        self.weights.clear();
        self.pairing.clear();
        self.verdicts.clear();
        for ((key, sig), root) in self.keys.iter().zip(&self.sigs).zip(&self.roots) {
            // An infinity key and signature pass for every message.
            let (Some(key), Some(sig)) = (key.filter(|key| !key.is_infinity()), sig) else {
                self.verdicts.push(false);
                continue;
            };
            self.included_keys.push(key);
            self.included_sigs.push(*sig);
            self.included_hashes.push(G2Point::hash_to_curve(root.as_slice(), BLS_DST));
            self.verdicts.push(true);
        }
        let batched = self.included_keys.len() > 1;
        if batched {
            let mut rng = rand::rng();
            for (key, hash) in self.included_keys.iter().zip(&self.included_hashes) {
                // Zero would omit the item; replacing it with one makes that weight twice as
                // likely.
                let weight = rng.random::<u64>().max(1).to_le_bytes();
                self.pairing.push(&G1Point::weighted_sum(&[*key], &weight), hash);
                self.weights.extend_from_slice(&weight);
            }
            let sum = G2Point::weighted_sum(&self.included_sigs, &self.weights);
            self.pairing.push(&G1Point::negative_generator(), &sum);
            if self.pairing.is_one() {
                return false;
            }
        }
        let included = self.verdicts.iter_mut().filter(|verdict| **verdict);
        let points = self.included_keys.iter().zip(&self.included_sigs).zip(&self.included_hashes);
        for (verdict, ((key, sig), hash)) in included.zip(points) {
            *verdict = PairingProduct::two_pairs_are_one(
                (&G1Point::negative_generator(), sig),
                (key, hash),
            );
        }
        batched
    }
}

pub fn signing_root(object_root: B256, domain: B256) -> B256 {
    B256::from(hash_concat(&object_root.0, &domain.0))
}

pub fn signed(message: &[u8], signature: &[u8; 96]) -> Vec<u8> {
    let mut signed = Vec::with_capacity(SIGNED_FIXED + message.len());
    signed.extend_from_slice(&(SIGNED_FIXED as u32).to_le_bytes());
    signed.extend_from_slice(signature);
    signed.extend_from_slice(message);
    signed
}

pub fn bid_root(bid: &[u8]) -> B256 {
    B256::from(ExecutionPayloadBidView::hash_tree_root(bid))
}

pub fn envelope_root(envelope: &[u8]) -> B256 {
    B256::from(ExecutionPayloadEnvelopeView::hash_tree_root(envelope))
}

pub fn bid_slot(bid: &[u8]) -> u64 {
    ExecutionPayloadBidView::slot(bid)
}

pub fn set_bid_builder_index(bid: &mut [u8], builder_index: u64) {
    bid[156..164].copy_from_slice(&builder_index.to_le_bytes());
}

pub struct GloasBlock<'a>(&'a [u8]);

impl<'a> GloasBlock<'a> {
    pub fn new(bytes: &'a [u8]) -> Option<Self> {
        (SignedBeaconBlockView::check_gloas_size(bytes) &&
            BeaconBlockBodyGloasView::check_canonical(SignedBeaconBlockView::body(bytes)))
        .then_some(Self(bytes))
    }

    pub fn slot(&self) -> u64 {
        SignedBeaconBlockView::slot(self.0)
    }

    pub fn parent_root(&self) -> B256 {
        B256::from(*SignedBeaconBlockView::parent_root(self.0))
    }

    pub fn signature(&self) -> &[u8; 96] {
        SignedBeaconBlockView::signature(self.0)
    }

    fn bid(&self) -> &'a [u8] {
        SignedBeaconBlockView::gloas_bid(self.0)
    }

    pub fn bid_block_hash(&self) -> B256 {
        B256::from(*ExecutionPayloadBidView::block_hash(self.bid()))
    }

    pub fn bid_builder_index(&self) -> u64 {
        ExecutionPayloadBidView::builder_index(self.bid())
    }

    pub fn block_root(&self) -> B256 {
        B256::from(merkleize(&[
            uint64_chunk(self.slot()),
            uint64_chunk(SignedBeaconBlockView::proposer_index(self.0)),
            *SignedBeaconBlockView::parent_root(self.0),
            *SignedBeaconBlockView::state_root(self.0),
            BeaconBlockBodyGloasView::hash_tree_root(SignedBeaconBlockView::body(self.0)),
        ]))
    }
}

fn payload_layout_ok(d: &[u8]) -> bool {
    if d.len() < EXECUTION_PAYLOAD_FIXED_GLOAS {
        return false;
    }
    let extra = ExecutionPayloadView::extra_data_offset(d) as usize;
    let txs = ExecutionPayloadView::transactions_offset(d) as usize;
    let withdrawals = ExecutionPayloadView::withdrawals_offset(d) as usize;
    let bal = ExecutionPayloadView::block_access_list_offset(d) as usize;
    extra == EXECUTION_PAYLOAD_FIXED_GLOAS &&
        extra <= txs &&
        txs <= withdrawals &&
        withdrawals <= bal &&
        bal <= d.len() &&
        txs - extra <= MAX_EXTRA_DATA_BYTES &&
        (bal - withdrawals).is_multiple_of(WITHDRAWAL_SIZE) &&
        (bal - withdrawals) / WITHDRAWAL_SIZE <= MAX_WITHDRAWALS_PER_PAYLOAD &&
        transactions_layout_ok(&d[txs..withdrawals])
}

fn transactions_layout_ok(txs: &[u8]) -> bool {
    if txs.is_empty() {
        return true;
    }
    if txs.len() < 4 {
        return false;
    }
    let first = u32_at(txs, 0);
    if !first.is_multiple_of(4) || first > txs.len() || first / 4 > MAX_TRANSACTIONS_PER_PAYLOAD {
        return false;
    }
    let count = first / 4;
    (0..count).all(|i| {
        let start = u32_at(txs, 4 * i);
        let end = if i + 1 < count { u32_at(txs, 4 * (i + 1)) } else { txs.len() };
        start <= end && end <= txs.len() && end - start <= MAX_BYTES_PER_TRANSACTION
    })
}

fn blobs_bundle_layout_ok(bundle: &[u8]) -> bool {
    if bundle.len() < 12 {
        return false;
    }
    let (commitments, proofs, blobs) = (u32_at(bundle, 0), u32_at(bundle, 4), u32_at(bundle, 8));
    if commitments != 12 || proofs < commitments || blobs < proofs || blobs > bundle.len() {
        return false;
    }
    let (commitments, proofs, blobs) = (proofs - commitments, blobs - proofs, bundle.len() - blobs);
    blobs.is_multiple_of(BYTES_PER_BLOB) &&
        commitments == blobs / BYTES_PER_BLOB * BYTES_PER_KZG_COMMITMENT &&
        proofs == blobs / BYTES_PER_BLOB * CELLS_PER_EXT_BLOB * BYTES_PER_KZG_PROOF
}

#[cfg(test)]
mod tests {
    use alloy_primitives::hex;
    use blst::min_pk;
    use lh_bls::{AggregateSignature, INFINITY_PUBLIC_KEY, INFINITY_SIGNATURE};

    use super::*;
    use crate::{BlsPublicKey, BlsSignature, get_fixed_pubkey, get_fixed_secret};

    /// On-curve point outside the G2 subgroup, from batch 93 of
    /// `crates/crypto_simd/proof/decompress_g2_vectors.txt` in silver
    /// at tag `silver_crypto_simd-v0.1.0`.
    const OUTSIDE_G2: [u8; 96] = hex!(
        "82ad35e7521bc55f76767951977505562e3d2ccbe916a4eec9eb039471d8a2e1"
        "8e48a60704f3f49873ffff81994ff4750d34624ab262ab1d0c494c944408f4ca"
        "e3b2624a1218391c71e5f22a3878fc12330cca92d6f7233e5ff5a9c125ebf647"
    );

    /// Nonzero point of order 13 on the BLS12-381 twist.
    /// Derived as `(h₂/13²)·r·OUTSIDE_G2`, where 13² divides the G2 cofactor h₂.
    const ORDER_13: [u8; 96] = hex!(
        "a3a58fd8e0c4b1a4025da5ca3176405ed7fa11dcf55003b1ceae958a7a62815f"
        "5f3a79792d91eebf83ccffc228784a4509c801b1c24e6662e6c94f9b8f0ac017"
        "68d0b974fe1ad3d3727c7afc77bafd60e086aeed8a678de31267be27fd73543b"
    );

    #[test]
    fn verdicts_match_lighthouse() {
        let root = B256::repeat_byte(1);
        let (pk, sig) =
            (get_fixed_pubkey(0).serialize(), get_fixed_secret(0).sign(root).serialize());
        let mut flagless = sig;
        flagless[0] &= 0x7f;
        let check = |case: &str, pubkey: &[u8], signature: &[u8], root: B256, valid: bool| {
            assert_eq!(bls_verify(pubkey, signature, root).is_ok(), valid, "{case}");
            let lighthouse = BlsPublicKey::deserialize(pubkey)
                .and_then(|pubkey| Ok(BlsSignature::deserialize(signature)?.verify(&pubkey, root)));
            assert_eq!(lighthouse.unwrap_or(false), valid, "{case}: lighthouse");
        };
        let outside = BlsSignature::deserialize(&OUTSIDE_G2).expect("OUTSIDE_G2 is on the curve");
        assert!(!outside.point().unwrap().subgroup_check(), "OUTSIDE_G2 is outside G2");

        check("valid", &pk, &sig, root, true);
        check("wrong message", &pk, &sig, B256::repeat_byte(2), false);
        check("wrong key", &get_fixed_pubkey(1).serialize(), &sig, root, false);
        check("signature outside G2", &pk, &OUTSIDE_G2, root, false);
        check("all-zero signature", &pk, &[0; 96], root, false);
        check("missing compression flag", &pk, &flagless, root, false);
        check("infinity signature", &pk, &INFINITY_SIGNATURE, root, false);
        check("infinity key and signature", &INFINITY_PUBLIC_KEY, &INFINITY_SIGNATURE, root, false);
    }

    type Item = ([u8; 48], [u8; 96], B256);

    fn signed_item(key: usize, root: B256) -> Item {
        (get_fixed_pubkey(key).serialize(), get_fixed_secret(key).sign(root).serialize(), root)
    }

    fn verify_batch(items: &[Item]) -> (Vec<bool>, bool) {
        let mut batch = BlsBatch::default();
        for (pubkey, signature, root) in items {
            batch.push(pubkey, signature, *root);
        }
        let fell_back = batch.verify();
        (batch.verdicts().to_vec(), fell_back)
    }

    #[test]
    fn batch_flags_only_the_invalid_signature() {
        let mut items: Vec<_> =
            (0..4).map(|i| signed_item(i, B256::repeat_byte(i as u8))).collect();
        items[2].2 = B256::repeat_byte(9);

        assert_eq!(verify_batch(&items), (vec![true, true, false, true], true));
    }

    #[test]
    fn batch_flags_signatures_whose_errors_cancel() {
        let (root0, root1) = (B256::repeat_byte(1), B256::repeat_byte(2));
        let (sig0, sig1) = (get_fixed_secret(0).sign(root0), get_fixed_secret(1).sign(root1));
        // Swapping adds `sig1 - sig0` to one signature and subtracts it from the other.
        let mut sum = AggregateSignature::infinity();
        sum.add_assign(&sig1);
        sum.add_assign(&sig0);
        let keys = [get_fixed_pubkey(0), get_fixed_pubkey(1)];
        assert!(sum.aggregate_verify(&[root0, root1], &[&keys[0], &keys[1]]), "unweighted sum");

        let items = [
            (keys[0].serialize(), sig1.serialize(), root0),
            (keys[1].serialize(), sig0.serialize(), root1),
            signed_item(2, B256::repeat_byte(3)),
        ];
        assert_eq!(verify_batch(&items).0, [false, false, true]);
    }

    #[test]
    fn batch_excludes_signature_with_small_order_component() {
        let component = min_pk::Signature::from_bytes(&ORDER_13).expect("on the curve");
        assert!(!component.subgroup_check(), "outside G2, so not infinity");
        let thirteen =
            min_pk::AggregateSignature::aggregate_with_randomness(&[component], &[13], 8, false);
        assert_eq!(thirteen.unwrap().to_signature().to_bytes(), INFINITY_SIGNATURE, "order 13");

        let (pk, sig, root) = signed_item(0, B256::repeat_byte(1));
        let sig = min_pk::Signature::from_bytes(&sig).unwrap();
        let tampered = min_pk::AggregateSignature::aggregate(&[&sig, &component], false).unwrap();
        let items = [
            (pk, tampered.to_signature().to_bytes(), root),
            signed_item(1, B256::repeat_byte(2)),
            signed_item(2, B256::repeat_byte(3)),
        ];
        // A subgroup check on the weighted sum misses this component when 13 divides its weight.
        // The false verdict without fallback distinguishes decoding rejection from a failed
        // pairing.
        assert_eq!(verify_batch(&items), (vec![false, true, true], false));
    }

    #[test]
    fn batch_flags_infinity() {
        let items = [
            signed_item(0, B256::repeat_byte(1)),
            (INFINITY_PUBLIC_KEY, INFINITY_SIGNATURE, B256::repeat_byte(2)),
            signed_item(1, B256::repeat_byte(3)),
        ];
        assert_eq!(verify_batch(&items).0, [true, false, true]);

        let items: Vec<_> = (0..3)
            .map(|i| {
                (get_fixed_pubkey(i).serialize(), INFINITY_SIGNATURE, B256::repeat_byte(i as u8))
            })
            .collect();
        assert_eq!(verify_batch(&items).0, [false; 3]);
    }

    #[test]
    fn reused_batch_accepts_valid_signatures_in_one_check() {
        let mut batch = BlsBatch::default();
        let mut first = [1, 2, 3].map(|i| signed_item(i, B256::repeat_byte(i as u8)));
        first[1].2 = B256::repeat_byte(9);
        for (pubkey, signature, root) in first {
            batch.push(&pubkey, &signature, root);
        }
        assert!(batch.verify(), "the first job falls back");

        for len in [9, 2, 1] {
            batch.clear();
            for i in 0..len {
                let (pubkey, signature, root) = signed_item(i % 5, B256::repeat_byte(i as u8 + 10));
                batch.push(&pubkey, &signature, root);
            }
            assert!(!batch.verify(), "{len} items pass without fallback");
            assert_eq!(batch.verdicts(), vec![true; len]);
        }
    }

    #[test]
    fn batch_verdicts_match_single_verify() {
        let root = B256::repeat_byte(1);
        let (pk, sig) =
            (get_fixed_pubkey(0).serialize(), get_fixed_secret(0).sign(root).serialize());
        let mut flagless = sig;
        flagless[0] &= 0x7f;
        let mut items = vec![
            (pk, sig, root),
            (pk, sig, B256::repeat_byte(2)),
            (get_fixed_pubkey(1).serialize(), sig, root),
            (pk, OUTSIDE_G2, root),
            (pk, [0; 96], root),
            (pk, flagless, root),
            (pk, INFINITY_SIGNATURE, root),
            (INFINITY_PUBLIC_KEY, INFINITY_SIGNATURE, root),
            ([0; 48], sig, root),
        ];
        items.extend((1..5).map(|i| signed_item(i, B256::repeat_byte(i as u8 + 10))));
        let single: Vec<_> =
            items.iter().map(|(pk, sig, root)| bls_verify(pk, sig, *root).is_ok()).collect();
        assert!(single.contains(&true) && single.contains(&false), "mixed fixture");

        assert_eq!(verify_batch(&items).0, single);
        for (item, valid) in items.iter().zip(single) {
            assert_eq!(verify_batch(&[*item, (pk, [0; 96], root)]), (vec![valid, false], false));
        }
    }
}
