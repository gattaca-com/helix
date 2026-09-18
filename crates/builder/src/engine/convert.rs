//! Byte-level bridges between the wire types (alloy, via `helix-tcp-types`)
//! and ethrex's `ethereum-types`-based primitives. The two type families never
//! meet in one struct: everything crosses this boundary explicitly.

use alloy_primitives::{Address as AAddress, B256, Bloom as ABloom, U256 as AU256};
use alloy_rpc_types::{
    beacon::requests::ExecutionRequestsV4,
    engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3},
};
use ethrex_common::{
    Address as EAddress, H256, U256 as EU256,
    constants::DEFAULT_OMMERS_HASH,
    types::{
        Block, BlockBody, BlockHeader, Transaction, Withdrawal, compute_transactions_root,
        compute_withdrawals_root,
        requests::{EncodedRequests, compute_requests_hash},
    },
};
use ethrex_crypto::NativeCrypto;
use helix_types::{BuilderDepositRequests, BuilderExitRequests, ExecutionRequests, RequestType};
use ssz::Encode;

use crate::validation::error::ValidationError;

pub fn h256(b: B256) -> H256 {
    H256(b.0)
}

pub fn b256(h: H256) -> B256 {
    B256::new(h.0)
}

pub fn eaddr(a: AAddress) -> EAddress {
    EAddress::from_slice(a.as_slice())
}

pub fn aaddr(a: EAddress) -> AAddress {
    AAddress::from_slice(a.as_bytes())
}

pub fn au256(v: EU256) -> AU256 {
    AU256::from_be_bytes(v.to_big_endian())
}

pub fn eu256(v: AU256) -> EU256 {
    EU256::from_big_endian(&v.to_be_bytes::<32>())
}

pub fn ewithdrawal(w: &alloy_eips::eip4895::Withdrawal) -> Withdrawal {
    Withdrawal {
        index: w.index,
        validator_index: w.validator_index,
        address: eaddr(w.address),
        amount: w.amount,
    }
}

pub fn awithdrawal(w: &Withdrawal) -> alloy_eips::eip4895::Withdrawal {
    alloy_eips::eip4895::Withdrawal {
        index: w.index,
        validator_index: w.validator_index,
        address: aaddr(w.address),
        amount: w.amount,
    }
}

/// Converts a finalized ethrex block into the wire `ExecutionPayloadV3`.
pub fn block_to_payload_v3(block: &Block) -> ExecutionPayloadV3 {
    let header = &block.header;
    let transactions =
        block.body.transactions.iter().map(|tx| tx.encode_canonical_to_vec().into()).collect();
    let withdrawals =
        block.body.withdrawals.as_deref().unwrap_or_default().iter().map(awithdrawal).collect();

    ExecutionPayloadV3 {
        payload_inner: ExecutionPayloadV2 {
            payload_inner: ExecutionPayloadV1 {
                parent_hash: b256(header.parent_hash),
                fee_recipient: aaddr(header.coinbase),
                state_root: b256(header.state_root),
                receipts_root: b256(header.receipts_root),
                logs_bloom: ABloom::new(header.logs_bloom.0),
                prev_randao: b256(header.prev_randao),
                block_number: header.number,
                gas_limit: header.gas_limit,
                gas_used: header.gas_used,
                timestamp: header.timestamp,
                extra_data: header.extra_data.clone().into(),
                base_fee_per_gas: AU256::from(header.base_fee_per_gas.unwrap_or_default()),
                block_hash: b256(block.hash()),
                transactions,
            },
            withdrawals,
        },
        blob_gas_used: header.blob_gas_used.unwrap_or_default(),
        excess_blob_gas: header.excess_blob_gas.unwrap_or_default(),
    }
}

/// The Amsterdam header inputs no `ExecutionPayloadV3` carries: the EIP-7928
/// block access list, the EIP-7843 slot number, and the EIP-8282 builder
/// request lists that the `requests_hash` commits to. `None` for an earlier
/// fork.
#[derive(Clone, Copy, Default)]
pub struct Amsterdam<'a> {
    /// The list as the builder encoded it. It is hashed as received and never
    /// re-encoded, because the block hash commits to these exact bytes.
    pub block_access_list: &'a [u8],
    pub slot: u64,
    pub builder_deposits: Option<&'a BuilderDepositRequests>,
    pub builder_exits: Option<&'a BuilderExitRequests>,
}

/// Inverse of [`block_to_payload_v3`]. The roots the payload omits are
/// recomputed, so the hash this yields is the one the submission is bound to.
#[allow(dead_code)]
pub fn payload_v3_to_block(
    payload: &ExecutionPayloadV3,
    parent_beacon_block_root: B256,
    requests: &ExecutionRequestsV4,
    amsterdam: Option<Amsterdam<'_>>,
) -> Result<Block, ValidationError> {
    // An empty list hashes to something no builder committed to, which would
    // surface as a block hash mismatch. Name the real fault instead.
    if amsterdam.is_some_and(|a| a.block_access_list.is_empty()) {
        return Err(ValidationError::EmptyBlockAccessList);
    }
    let inner = &payload.payload_inner.payload_inner;

    let transactions = inner
        .transactions
        .iter()
        .map(|encoded| Transaction::decode_canonical(encoded))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ValidationError::DecodeTransaction(e.to_string()))?;
    let withdrawals: Vec<Withdrawal> =
        payload.payload_inner.withdrawals.iter().map(ewithdrawal).collect();

    let base_fee_per_gas: u64 =
        inner.base_fee_per_gas.try_into().map_err(|_| ValidationError::BaseFeeTooLarge)?;

    let header = BlockHeader {
        parent_hash: h256(inner.parent_hash),
        ommers_hash: *DEFAULT_OMMERS_HASH,
        coinbase: eaddr(inner.fee_recipient),
        state_root: h256(inner.state_root),
        transactions_root: compute_transactions_root(&transactions, &NativeCrypto),
        receipts_root: h256(inner.receipts_root),
        logs_bloom: ethrex_common::Bloom(inner.logs_bloom.0.0),
        difficulty: EU256::zero(),
        number: inner.block_number,
        gas_limit: inner.gas_limit,
        gas_used: inner.gas_used,
        timestamp: inner.timestamp,
        extra_data: inner.extra_data.0.clone(),
        prev_randao: h256(inner.prev_randao),
        nonce: 0,
        base_fee_per_gas: Some(base_fee_per_gas),
        withdrawals_root: Some(compute_withdrawals_root(&withdrawals, &NativeCrypto)),
        blob_gas_used: Some(payload.blob_gas_used),
        excess_blob_gas: Some(payload.excess_blob_gas),
        parent_beacon_block_root: Some(h256(parent_beacon_block_root)),
        requests_hash: Some(compute_requests_hash(&encoded_requests_all(requests, amsterdam))),
        block_access_list_hash: amsterdam
            .map(|a| ethrex_common::utils::keccak(a.block_access_list)),
        slot_number: amsterdam.map(|a| a.slot),
        ..Default::default()
    };

    let body = BlockBody { transactions, ommers: vec![], withdrawals: Some(withdrawals) };
    Ok(Block::new(header, body))
}

/// The wire bundle carries EIP-7594 cell proofs, so the ethrex bundle is
/// version 1.
pub fn eblobs(
    bundle: &alloy_rpc_types::engine::BlobsBundleV2,
) -> ethrex_common::types::BlobsBundle {
    ethrex_common::types::BlobsBundle {
        // A blob is 128 KiB. Fill the vector in place: collecting through an
        // iterator moves each blob across the stack.
        blobs: {
            let mut blobs = vec![[0u8; ethrex_common::types::BYTES_PER_BLOB]; bundle.blobs.len()];
            for (out, blob) in blobs.iter_mut().zip(bundle.blobs.iter()) {
                out.copy_from_slice(blob.as_slice());
            }
            blobs
        },
        commitments: bundle
            .commitments
            .iter()
            .map(|c| ethrex_common::types::Commitment::from(c.0))
            .collect(),
        proofs: bundle.proofs.iter().map(|p| ethrex_common::types::Proof::from(p.0)).collect(),
        version: 1,
    }
}

/// Inverse of [`requests_to_v4`]. `compute_requests_hash` skips type-byte-only
/// entries, so the empty ones the wire format drops need not be restored.
fn encoded_requests(requests: &ExecutionRequestsV4) -> Vec<EncodedRequests> {
    requests.to_requests().iter().map(|request| EncodedRequests(request.clone().0.into())).collect()
}

/// The flat EIP-7685 list the header's `requests_hash` commits to. EIP-8282's
/// builder lists carry types 3 and 4, so they follow the three alloy encodes,
/// and an empty list is omitted exactly as the EIP requires.
fn encoded_requests_all(
    requests: &ExecutionRequestsV4,
    amsterdam: Option<Amsterdam<'_>>,
) -> Vec<EncodedRequests> {
    let mut list = encoded_requests(requests);
    let mut push = |request_type: RequestType, body: Vec<u8>, is_empty: bool| {
        if is_empty {
            return;
        }
        let mut bytes = Vec::with_capacity(1 + body.len());
        bytes.push(request_type.to_u8());
        bytes.extend_from_slice(&body);
        list.push(EncodedRequests(bytes.into()));
    };
    if let Some(deposits) = amsterdam.and_then(|a| a.builder_deposits) {
        push(RequestType::BuilderDeposit, deposits.as_ssz_bytes(), deposits.is_empty());
    }
    if let Some(exits) = amsterdam.and_then(|a| a.builder_exits) {
        push(RequestType::BuilderExit, exits.as_ssz_bytes(), exits.is_empty());
    }
    list
}

/// Converts ethrex's encoded EIP-7685 requests into the wire
/// `ExecutionRequestsV4`, dropping empty requests per the EIP.
pub fn requests_to_v4(encoded: &[EncodedRequests]) -> Result<ExecutionRequestsV4, String> {
    let requests = alloy_eips::eip7685::Requests::new(
        encoded.iter().filter(|r| !r.is_empty()).map(|r| r.0.clone().to_vec().into()).collect(),
    );
    ExecutionRequestsV4::try_from(&requests).map_err(|e| e.to_string())
}

/// Every execution request the node produced, split by EIP-7685 type prefix.
/// The three pre-Gloas lists keep the shape a submission has always carried;
/// EIP-8282's builder deposits and exits ride beside them.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DecodedRequests {
    pub requests: ExecutionRequests,
    pub builder_deposits: BuilderDepositRequests,
    pub builder_exits: BuilderExitRequests,
}

/// Decodes the node's flat EIP-7685 list. Each entry is a one-byte type prefix
/// followed by the SSZ list for that type.
///
/// This does not go through alloy's `ExecutionRequestsV4`: that type knows only
/// prefixes 0 to 2 and rejects EIP-8282's 3 and 4, which cost the builder every
/// block carrying one.
pub fn decode_execution_requests(encoded: &[EncodedRequests]) -> Result<DecodedRequests, String> {
    let mut decoded = DecodedRequests::default();

    for entry in encoded.iter().filter(|r| !r.is_empty()) {
        let (prefix, body) = entry.0.split_first().ok_or("empty execution request")?;
        let request_type = RequestType::from_u8(*prefix)
            .ok_or_else(|| format!("unknown request_type prefix: {prefix}"))?;

        match request_type {
            RequestType::Deposit => {
                decoded.requests.deposits = ssz_decode(body, "deposits")?;
            }
            RequestType::Withdrawal => {
                decoded.requests.withdrawals = ssz_decode(body, "withdrawals")?;
            }
            RequestType::Consolidation => {
                decoded.requests.consolidations = ssz_decode(body, "consolidations")?;
            }
            RequestType::BuilderDeposit => {
                decoded.builder_deposits = ssz_decode(body, "builder deposits")?;
            }
            RequestType::BuilderExit => {
                decoded.builder_exits = ssz_decode(body, "builder exits")?;
            }
        }
    }

    Ok(decoded)
}

fn ssz_decode<T: ssz::Decode>(body: &[u8], what: &str) -> Result<T, String> {
    T::from_ssz_bytes(body).map_err(|e| format!("{what}: {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u256_conversion() {
        for v in [0u128, 1, u64::MAX as u128, u128::MAX] {
            assert_eq!(au256(EU256::from(v)), AU256::from(v));
        }
        assert_eq!(au256(EU256::max_value()), AU256::MAX);
    }

    /// Production shape: the node emits EIP-8282 builder deposits and exits, and
    /// the block was unbiddable while the decoder rejected prefix 3.
    #[test]
    fn the_flat_list_decodes_every_eip_7685_type() {
        use ssz::Encode;
        let deposits: BuilderDepositRequests = vec![helix_types::BuilderDepositRequest {
            pubkey: helix_types::BlsPublicKeyBytesLh::empty(),
            withdrawal_credentials: B256::repeat_byte(0x11),
            amount: 32_000_000_000,
            signature: helix_types::BlsSignatureBytesLh::empty(),
        }]
        .into();
        let exits: BuilderExitRequests = vec![helix_types::BuilderExitRequest {
            source_address: AAddress::repeat_byte(0x22),
            pubkey: helix_types::BlsPublicKeyBytesLh::empty(),
        }]
        .into();
        let encoded = vec![
            prefixed(RequestType::BuilderDeposit, deposits.as_ssz_bytes()),
            prefixed(RequestType::BuilderExit, exits.as_ssz_bytes()),
        ];

        let decoded = decode_execution_requests(&encoded).expect("prefixes 3 and 4 are valid");

        assert_eq!(decoded.builder_deposits, deposits);
        assert_eq!(decoded.builder_exits, exits);
    }

    #[test]
    fn an_unknown_request_type_is_named() {
        let err = decode_execution_requests(&[EncodedRequests(vec![9u8, 0u8].into())])
            .expect_err("prefix 9 is not an EIP-7685 type");

        assert!(err.contains("unknown request_type prefix: 9"), "got: {err}");
    }

    fn prefixed(request_type: RequestType, body: Vec<u8>) -> EncodedRequests {
        let mut bytes = vec![request_type.to_u8()];
        bytes.extend_from_slice(&body);
        EncodedRequests(bytes.into())
    }

    #[test]
    fn hash_and_address_roundtrip() {
        let b = B256::repeat_byte(0xab);
        assert_eq!(b256(h256(b)), b);
        let a = AAddress::repeat_byte(0xcd);
        assert_eq!(aaddr(eaddr(a)), a);
    }
}
