//! Safe `multiSend` revenue-distribution: calldata construction, EIP-712 Safe
//! transaction signing and revenue-split math. Ported from the reth-based
//! merge engine in `crates/simulator/src/block_merging/mod.rs` so the
//! accounting matches the relay exactly; everything here is pure alloy + std.

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, U256, U512, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolValue, sol};
use helix_tcp_types::merging::control::RelayConfigV1;
use rustc_hash::FxHashMap;

use crate::engine::{error::MergeError, types::OriginRevenue};

const DELEGATE_CALL: u8 = 1;

/// Revenue split policy in basis points; the proposer implicitly receives
/// `TOTAL_BPS - sum` and the winning builder additionally keeps everything not
/// distributed. Snapshot of `RelayConfigV1` taken at `SlotStart`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistributionConfig {
    pub relay_bps: u64,
    pub merged_builder_bps: u64,
    pub winning_builder_bps: u64,
}

impl DistributionConfig {
    const TOTAL_BPS: u64 = 10_000;

    pub fn from_relay_config(config: &RelayConfigV1) -> Self {
        Self {
            relay_bps: config.relay_bps,
            merged_builder_bps: config.merged_builder_bps,
            winning_builder_bps: config.winning_builder_bps,
        }
    }

    fn split(&self, bps: u64, revenue: U256) -> U256 {
        (U256::from(bps) * revenue) / U256::from(Self::TOTAL_BPS)
    }

    fn relay_split(&self, revenue: U256) -> U256 {
        self.split(self.relay_bps, revenue)
    }

    pub fn proposer_split(&self, revenue: U256) -> U256 {
        let proposer_bps =
            Self::TOTAL_BPS - self.relay_bps - self.merged_builder_bps - self.winning_builder_bps;
        self.split(proposer_bps, revenue)
    }

    fn merged_builder_split(&self, revenue: U256) -> U256 {
        self.split(self.merged_builder_bps, revenue)
    }
}

/// Computes the revenue distribution, splitting merged-block revenue among the
/// participants after subtracting the estimated payment cost. Returns a map
/// from address to the value the distribution tx sends there; the block
/// beneficiary (winning builder) is excluded — it keeps the remainder.
pub fn prepare_revenues(
    distribution_config: &DistributionConfig,
    revenues: &FxHashMap<Address, OriginRevenue>,
    estimated_payment_cost: U256,
    proposer_fee_recipient: Address,
    relay_fee_recipient: Address,
    block_beneficiary: Address,
) -> FxHashMap<Address, U256> {
    let mut updated_revenues =
        FxHashMap::with_capacity_and_hasher(revenues.len() + 2, Default::default());

    let total_revenue: U256 = revenues.values().map(|v| v.revenue).sum();
    // Subtract the payment cost from the revenue
    let expected_revenue = total_revenue - estimated_payment_cost;

    let proposer_revenue = distribution_config.proposer_split(expected_revenue);
    updated_revenues
        .entry(proposer_fee_recipient)
        .and_modify(|v| *v += proposer_revenue)
        .or_insert(proposer_revenue);

    let relay_revenue = distribution_config.relay_split(expected_revenue);
    updated_revenues
        .entry(relay_fee_recipient)
        .and_modify(|v| *v += relay_revenue)
        .or_insert(relay_revenue);

    // The winning builder controls the beneficiary address and receives any
    // undistributed revenue, so it gets no explicit allocation.

    // Divide the revenue among the different order origins, scaled down by the
    // payment cost each origin implicitly shares.
    for (origin, origin_revenue) in revenues {
        let actualized_revenue = (origin_revenue.revenue.widening_mul(expected_revenue) /
            U512::from(total_revenue))
        .to();
        let builder_revenue = distribution_config.merged_builder_split(actualized_revenue);
        updated_revenues
            .entry(*origin)
            .and_modify(|v| *v += builder_revenue)
            .or_insert(builder_revenue);
    }

    // Just in case, remove the beneficiary address from the distribution.
    updated_revenues.remove(&block_beneficiary);

    updated_revenues
}

/// Everything the distribution tx needs from the live block state.
pub struct PaymentInputs {
    pub safe: Address,
    pub safe_balance: U256,
    pub safe_nonce: u64,
    pub signer_nonce: u64,
    pub chain_id: u64,
    /// Transaction gas limit for the payment tx.
    pub gas_limit: u64,
    pub base_fee_per_gas: u128,
    pub multisend_contract: Address,
}

/// Builds and signs the relay-signed EIP-1559 transaction that calls the
/// collateral Safe's `execTransaction` -> `multiSend` delegatecall. Returns
/// the canonical (2718) encoding.
pub fn build_payment_tx(
    signer: &PrivateKeySigner,
    inputs: &PaymentInputs,
    updated_revenues: &FxHashMap<Address, U256>,
) -> Result<Vec<u8>, MergeError> {
    // The safeTxGas parameter tells the Safe contract how much gas the internal
    // transaction should have; 80% of the tx gas limit leaves margin for the
    // Safe's own overhead (signature verification etc).
    let safe_tx_gas = inputs.gas_limit.saturating_mul(80) / 100;

    let calldata = encode_multisend_calldata(
        updated_revenues,
        inputs.safe,
        inputs.safe_balance,
        inputs.safe_nonce,
        inputs.multisend_contract,
        U256::from(safe_tx_gas),
        inputs.chain_id,
        signer,
    )?;

    let multisend_tx = TxEip1559 {
        chain_id: inputs.chain_id,
        nonce: inputs.signer_nonce,
        gas_limit: inputs.gas_limit,
        max_fee_per_gas: inputs.base_fee_per_gas,
        max_priority_fee_per_gas: 0,
        to: inputs.safe.into(),
        value: U256::ZERO,
        access_list: Default::default(),
        input: calldata.into(),
    };

    let signature = signer
        .sign_hash_sync(&multisend_tx.signature_hash())
        .expect("signer is local and private key is valid");
    let signed = multisend_tx.into_signed(signature);
    Ok(alloy_consensus::TxEnvelope::from(signed).encoded_2718())
}

/// Encodes a Safe `execTransaction` call that delegatecalls the multisend
/// contract to pay every recipient.
pub fn encode_multisend_calldata(
    value_by_recipient: &FxHashMap<Address, U256>,
    safe: Address,
    safe_balance: U256,
    safe_nonce: u64,
    multisend_call_only: Address,
    safe_tx_gas: U256,
    chain_id: u64,
    safe_owner_signer: &PrivateKeySigner,
) -> Result<Vec<u8>, MergeError> {
    sol! {
        function multiSend(bytes transactions) external payable;

        function execTransaction(
            address to,
            uint256 value,
            bytes data,
            uint8 operation,
            uint256 safeTxGas,
            uint256 baseGas,
            uint256 gasPrice,
            address gasToken,
            address refundReceiver,
            bytes signatures
        ) external returns (bool);
    }

    let multisend_payload = build_multisend_payload(value_by_recipient);
    let multisend_calldata = multiSendCall { transactions: multisend_payload.into() }.abi_encode();
    let total_value: U256 = value_by_recipient.values().sum();

    if safe_balance < total_value {
        return Err(MergeError::NoBalanceInBuilderSafe {
            address: safe,
            current: safe_balance,
            required: total_value,
        });
    }

    let safe_tx_hash = compute_safe_tx_hash(
        safe,
        multisend_call_only,
        total_value,
        &multisend_calldata,
        safe_tx_gas,
        safe_nonce,
        chain_id,
    );

    let sig_bytes = sign_safe_transaction(safe_owner_signer, safe_tx_hash);

    let call_data = execTransactionCall {
        to: multisend_call_only,
        value: total_value,
        data: multisend_calldata.into(),
        operation: DELEGATE_CALL,
        safeTxGas: safe_tx_gas,
        baseGas: U256::ZERO,
        gasPrice: U256::ZERO,
        gasToken: Address::ZERO,
        refundReceiver: Address::ZERO,
        signatures: sig_bytes.into(),
    }
    .abi_encode();

    Ok(call_data)
}

fn sign_safe_transaction(signer: &PrivateKeySigner, safe_tx_hash: B256) -> Vec<u8> {
    let signature =
        signer.sign_hash_sync(&safe_tx_hash).expect("signer is local and private key is valid");

    // Format signature for Safe (r, s, v)
    let mut sig_bytes = Vec::with_capacity(65);
    sig_bytes.extend_from_slice(&signature.r().to_be_bytes::<32>());
    sig_bytes.extend_from_slice(&signature.s().to_be_bytes::<32>());
    // Convert y_parity (bool) to v byte: 27 + y_parity (0 or 1)
    sig_bytes.push(27 + signature.v() as u8);

    sig_bytes
}

fn build_multisend_payload(value_by_recipient: &FxHashMap<Address, U256>) -> Vec<u8> {
    let mut payload = Vec::with_capacity(value_by_recipient.len() * 85);

    for (recipient, amount) in value_by_recipient {
        // operation = CALL (0)
        payload.push(0u8);
        // to (20 bytes)
        payload.extend_from_slice(recipient.as_slice());
        // value (uint256)
        payload.extend_from_slice(&amount.to_be_bytes::<32>());
        // calldata length = 0
        payload.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
    }

    payload
}

fn compute_safe_tx_hash(
    safe: Address,
    to: Address,
    value: U256,
    data: &[u8],
    safe_tx_gas: U256,
    nonce: u64,
    chain_id: u64,
) -> B256 {
    let domain_separator = {
        let domain_type_hash = keccak256("EIP712Domain(uint256 chainId,address verifyingContract)");
        keccak256((domain_type_hash, U256::from(chain_id), safe).abi_encode())
    };

    let safe_tx_type_hash = keccak256(
        "SafeTx(address to,uint256 value,bytes data,uint8 operation,uint256 safeTxGas,uint256 baseGas,uint256 gasPrice,address gasToken,address refundReceiver,uint256 nonce)",
    );

    let data_hash = keccak256(data);
    let safe_tx_hash = keccak256(
        (
            safe_tx_type_hash,
            to,
            value,
            data_hash,
            U256::from(DELEGATE_CALL),
            safe_tx_gas,
            U256::ZERO,    // baseGas
            U256::ZERO,    // gasPrice
            Address::ZERO, // gasToken
            Address::ZERO, // refundReceiver
            U256::from(nonce),
        )
            .abi_encode(),
    );

    // EIP-712 final hash
    // Note: Must use concatenation (encodePacked), not ABI encoding!
    let mut final_hash_data = Vec::with_capacity(2 + 32 + 32);
    final_hash_data.push(0x19u8);
    final_hash_data.push(0x01u8);
    final_hash_data.extend_from_slice(domain_separator.as_slice());
    final_hash_data.extend_from_slice(safe_tx_hash.as_slice());
    keccak256(final_hash_data)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy_primitives::{address, b256};

    use super::*;

    fn origin_revenue(revenue: U256) -> OriginRevenue {
        OriginRevenue { revenue, txs: vec![], pubkey: Default::default() }
    }

    #[test]
    fn test_prepare_revenues() {
        let distribution_config = DistributionConfig {
            relay_bps: 2500,
            merged_builder_bps: 2500,
            winning_builder_bps: 2500,
        };
        let winning_builder_fee_recipient = address!("0x0000000000000000000000000000000000000001");
        let relay_fee_recipient = address!("0x0000000000000000000000000000000000000002");
        let proposer_fee_recipient = address!("0x0000000000000000000000000000000000000003");

        let addresses = [
            address!("0x0000000000000000000000000000000000000006"),
            address!("0x0000000000000000000000000000000000000007"),
        ];
        let values = [U256::from(10000), U256::from(30000)];

        let revenues = FxHashMap::from_iter(
            addresses.iter().cloned().zip(values.iter().cloned().map(origin_revenue)),
        );
        let updated_revenues = prepare_revenues(
            &distribution_config,
            &revenues,
            U256::ZERO,
            proposer_fee_recipient,
            relay_fee_recipient,
            winning_builder_fee_recipient,
        );

        // Check total for relay + proposer + builders
        assert_eq!(updated_revenues.values().sum::<U256>(), U256::from(30000));
        // Check relay got 1/4 the total sum
        assert_eq!(updated_revenues[&relay_fee_recipient], U256::from(10000));
        // Check each merging builder got 1/4 their contribution
        assert_eq!(updated_revenues[&addresses[0]], U256::from(2500));
        assert_eq!(updated_revenues[&addresses[1]], U256::from(7500));
        // Check proposer value is 1/4 the total sum
        assert_eq!(updated_revenues[&proposer_fee_recipient], U256::from(10000));
        // Winning builder gets nothing assigned: the remainder is theirs anyway
        assert!(!updated_revenues.contains_key(&winning_builder_fee_recipient));
    }

    #[test]
    fn test_prepare_revenues_with_small_values() {
        let distribution_config = DistributionConfig {
            relay_bps: 2500,
            merged_builder_bps: 2500,
            winning_builder_bps: 2500,
        };
        let winning_builder_fee_recipient = address!("0x0000000000000000000000000000000000000001");
        let relay_fee_recipient = address!("0x0000000000000000000000000000000000000002");
        let proposer_fee_recipient = address!("0x0000000000000000000000000000000000000003");

        let addresses = [
            address!("0x0000000000000000000000000000000000000006"),
            address!("0x0000000000000000000000000000000000000007"),
        ];
        let values = [U256::from(7), U256::from(5)];

        let revenues = FxHashMap::from_iter(
            addresses.iter().cloned().zip(values.iter().cloned().map(origin_revenue)),
        );
        let updated_revenues = prepare_revenues(
            &distribution_config,
            &revenues,
            U256::ZERO,
            proposer_fee_recipient,
            relay_fee_recipient,
            winning_builder_fee_recipient,
        );

        assert_eq!(updated_revenues.values().sum::<U256>(), U256::from(8));
        assert_eq!(updated_revenues[&proposer_fee_recipient], U256::from(3));
        assert_eq!(updated_revenues[&addresses[0]], U256::from(1));
        assert_eq!(updated_revenues[&addresses[1]], U256::from(1));
        assert_eq!(updated_revenues[&relay_fee_recipient], U256::from(3));
        assert!(!updated_revenues.contains_key(&winning_builder_fee_recipient));
    }

    #[test]
    fn test_sign_safe_transaction_format() {
        let private_key = "0x1234567890123456789012345678901234567890123456789012345678901234";
        let signer = PrivateKeySigner::from_str(private_key).unwrap();
        let test_hash = b256!("0000000000000000000000000000000000000000000000000000000000000001");

        let sig_bytes = sign_safe_transaction(&signer, test_hash);

        assert_eq!(sig_bytes.len(), 65);
        let v = sig_bytes[64];
        assert!(v == 27 || v == 28, "v value should be 27 or 28, got {v}");

        // Deterministic for the same input
        assert_eq!(sig_bytes, sign_safe_transaction(&signer, test_hash));
    }

    #[test]
    fn test_compute_safe_tx_hash() {
        let safe = address!("0x1111111111111111111111111111111111111111");
        let to = address!("0x2222222222222222222222222222222222222222");
        let value = U256::from(1000);
        let data = vec![0x12, 0x34, 0x56, 0x78];
        let safe_tx_gas = U256::from(100000);
        let nonce = 5u64;
        let chain_id = 1u64;

        let hash = compute_safe_tx_hash(safe, to, value, &data, safe_tx_gas, nonce, chain_id);
        let hash2 = compute_safe_tx_hash(safe, to, value, &data, safe_tx_gas, nonce, chain_id);
        assert_eq!(hash, hash2);

        let hash3 = compute_safe_tx_hash(safe, to, value, &data, safe_tx_gas, nonce + 1, chain_id);
        assert_ne!(hash, hash3);
    }

    #[test]
    fn test_multisend_payload_format() {
        let mut recipients = FxHashMap::default();
        recipients.insert(address!("0x1111111111111111111111111111111111111111"), U256::from(100));
        recipients.insert(address!("0x2222222222222222222222222222222222222222"), U256::from(200));

        let payload = build_multisend_payload(&recipients);

        // Each transaction in multisend is 85 bytes:
        // 1 (operation) + 20 (address) + 32 (value) + 32 (data length)
        assert_eq!(payload.len(), 85 * 2);
        assert_eq!(payload[0], 0);
        assert_eq!(payload[85], 0);
    }

    #[test]
    fn test_build_payment_tx_decodes_as_ethrex_tx() {
        let private_key = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signer = PrivateKeySigner::from_str(private_key).unwrap();
        let inputs = PaymentInputs {
            safe: address!("0x1111111111111111111111111111111111111111"),
            safe_balance: U256::from(10_000_000u64),
            safe_nonce: 3,
            signer_nonce: 7,
            chain_id: 1,
            gas_limit: 140_000,
            base_fee_per_gas: 1_000_000_000,
            multisend_contract: address!("0x2222222222222222222222222222222222222222"),
        };
        let mut revenues = FxHashMap::default();
        revenues
            .insert(address!("0x3333333333333333333333333333333333333333"), U256::from(1_000u64));

        let encoded = build_payment_tx(&signer, &inputs, &revenues).unwrap();
        let tx = ethrex_common::types::Transaction::decode_canonical(&encoded).unwrap();
        assert_eq!(tx.gas_limit(), 140_000);
        assert_eq!(
            tx.sender(&ethrex_crypto::native::NativeCrypto).unwrap(),
            crate::engine::convert::eaddr(signer.address())
        );
    }
}
