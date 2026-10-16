use alloy_primitives::{Address, B256};
use bytes::Bytes;
use silver_ssz::{
    merkle::{hash_concat, merkleize, uint64_chunk},
    ssz_hash_gloas::ExecutionRequestsView,
    ssz_view::{
        BYTES_PER_BLOB, BYTES_PER_KZG_COMMITMENT, BYTES_PER_KZG_PROOF, BeaconBlockBodyGloasView,
        EXECUTION_PAYLOAD_FIXED_GLOAS, ExecutionPayloadBidView, ExecutionPayloadEnvelopeView,
        ExecutionPayloadView, MAX_BYTES_PER_TRANSACTION, MAX_EXTRA_DATA_BYTES,
        MAX_TRANSACTIONS_PER_PAYLOAD, MAX_WITHDRAWALS_PER_PAYLOAD, SignedBeaconBlockView,
    },
};
use ssz::{Decode, DecodeError};
use tree_hash::TreeHash;

use crate::{
    BidTrace, BlockValidationError, GloasBidTrace, SszError, Withdrawals, error::SigError,
};

const GLOAS_BID_TRACE_SIZE: usize = 268;
const SUBMISSION_FIXED: usize = GLOAS_BID_TRACE_SIZE + 3 * 4 + 96;
const WITHDRAWAL_SIZE: usize = 44;
const CELLS_PER_EXT_BLOB: usize = 128;
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

    pub fn execution_requests(&self) -> &[u8] {
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
        Withdrawals::from_ssz_bytes(withdrawals)
            .map(|withdrawals| withdrawals.tree_hash_root())
            .unwrap_or_default()
    }

    fn bundle_section(&self, i: usize) -> &[u8] {
        let bundle = self.blobs_bundle();
        let start = u32_at(bundle, 4 * i);
        let end = if i < 2 { u32_at(bundle, 4 * (i + 1)) } else { bundle.len() };
        &bundle[start..end]
    }

    pub fn commitments(&self) -> &[u8] {
        self.bundle_section(0)
    }

    pub fn proofs(&self) -> &[u8] {
        self.bundle_section(1)
    }

    pub fn blobs(&self) -> &[u8] {
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
    let pubkey = blst::min_pk::PublicKey::key_validate(pubkey)
        .map_err(|_| SigError::InvalidBlsPubkeyBytes)?;
    let signature = blst::min_pk::Signature::from_bytes(signature)
        .map_err(|_| SigError::InvalidBlsSignatureBytes)?;
    match signature.verify(true, root.as_slice(), BLS_DST, &[], &pubkey, false) {
        blst::BLST_ERROR::BLST_SUCCESS => Ok(()),
        _ => Err(SigError::InvalidBlsSignature),
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

    pub fn bid(&self) -> &'a [u8] {
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
