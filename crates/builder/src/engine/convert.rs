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

/// Inverse of [`block_to_payload_v3`]. The roots the payload omits are
/// recomputed, so the hash this yields is the one the submission is bound to.
#[allow(dead_code)]
pub fn payload_v3_to_block(
    payload: &ExecutionPayloadV3,
    parent_beacon_block_root: B256,
    requests: &ExecutionRequestsV4,
) -> Result<Block, ValidationError> {
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
        requests_hash: Some(compute_requests_hash(&encoded_requests(requests))),
        ..Default::default()
    };

    let body = BlockBody { transactions, ommers: vec![], withdrawals: Some(withdrawals) };
    Ok(Block::new(header, body))
}

/// Inverse of [`requests_to_v4`]. `compute_requests_hash` skips type-byte-only
/// entries, so the empty ones the wire format drops need not be restored.
fn encoded_requests(requests: &ExecutionRequestsV4) -> Vec<EncodedRequests> {
    requests.to_requests().iter().map(|request| EncodedRequests(request.clone().0.into())).collect()
}

/// Converts ethrex's encoded EIP-7685 requests into the wire
/// `ExecutionRequestsV4`, dropping empty requests per the EIP.
pub fn requests_to_v4(encoded: &[EncodedRequests]) -> Result<ExecutionRequestsV4, String> {
    let requests = alloy_eips::eip7685::Requests::new(
        encoded.iter().filter(|r| !r.is_empty()).map(|r| r.0.clone().to_vec().into()).collect(),
    );
    ExecutionRequestsV4::try_from(&requests).map_err(|e| e.to_string())
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

    #[test]
    fn hash_and_address_roundtrip() {
        let b = B256::repeat_byte(0xab);
        assert_eq!(b256(h256(b)), b);
        let a = AAddress::repeat_byte(0xcd);
        assert_eq!(aaddr(eaddr(a)), a);
    }
}
