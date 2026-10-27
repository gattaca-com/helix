//! The distribution tx: merged revenue split per the relay's policy, paid from the base builder's
//! collateral Safe through `execTransaction` -> `multiSend` (delegatecall), signed by the relay's
//! Safe owner key. The split must match the relay's accounting exactly.

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, U256, U512, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolValue, sol};
use helix_tcp_types::merging::control::RelayConfigV1;

use crate::merge::ingest::Source;

const TOTAL_BPS: u64 = 10_000;
const DELEGATE_CALL: u8 = 1;

/// What one origin builder's orders paid the coinbase; `source` is the first of its sources to
/// pay, by index into the pool's `sources`, which are grouped by address.
pub struct Revenue {
    pub source: u16,
    pub paid: U256,
}

/// Recipient values of the distribution, after `cost` (the distribution's own gas) comes off the
/// total. The proposer and relay are always listed; the winning builder (`beneficiary`) never is,
/// it keeps whatever is not paid out. An address with several roles gets one summed entry.
pub fn split(
    relay: &RelayConfigV1,
    revenues: &[Revenue],
    sources: &[Source],
    cost: U256,
    proposer: Address,
    beneficiary: Address,
) -> Vec<(Address, U256)> {
    let total: U256 = revenues.iter().map(|r| r.paid).sum();
    let net = total - cost;
    let share = |bps: u64, value: U256| U256::from(bps) * value / U256::from(TOTAL_BPS);
    let proposer_bps =
        TOTAL_BPS - relay.relay_bps - relay.merged_builder_bps - relay.winning_builder_bps;

    let mut out: Vec<(Address, U256)> = Vec::with_capacity(revenues.len() + 2);
    let mut credit = |to: Address, value: U256| match out.iter_mut().find(|(a, _)| *a == to) {
        Some((_, v)) => *v += value,
        None => out.push((to, value)),
    };
    credit(proposer, share(proposer_bps, net));
    credit(relay.relay_fee_recipient, share(relay.relay_bps, net));
    for revenue in revenues {
        // Each origin carries its pro-rata part of the cost.
        let actual = (revenue.paid.widening_mul(net) / U512::from(total)).to::<U256>();
        let origin = sources[revenue.source as usize].address;
        credit(origin, share(relay.merged_builder_bps, actual));
    }
    out.retain(|(a, _)| *a != beneficiary);
    out
}

pub struct SafePayment {
    pub safe: Address,
    pub safe_balance: U256,
    pub safe_nonce: u64,
    pub signer_nonce: u64,
    pub chain_id: u64,
    pub gas_limit: u64,
    pub base_fee: u128,
    pub multisend: Address,
}

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

/// The signed EIP-1559 distribution tx, canonically encoded. Fails when the Safe cannot cover it.
pub fn payment_tx(
    signer: &PrivateKeySigner,
    p: &SafePayment,
    to: &[(Address, U256)],
) -> Result<Vec<u8>, &'static str> {
    let total: U256 = to.iter().map(|(_, v)| *v).sum();
    if p.safe_balance < total {
        return Err("safe_balance");
    }
    // multiSend packing per call: operation (CALL) | to | value | data length (0).
    let mut packed = Vec::with_capacity(to.len() * 85);
    for (recipient, value) in to {
        packed.push(0u8);
        packed.extend_from_slice(recipient.as_slice());
        packed.extend_from_slice(&value.to_be_bytes::<32>());
        packed.extend_from_slice(&[0u8; 32]);
    }
    let data = multiSendCall { transactions: packed.into() }.abi_encode();
    // Zero safeTxGas forwards all remaining gas and reverts the Safe call when the multisend
    // fails; a fixed share of the limit trips GS010.
    let hash = safe_tx_hash(p.safe, p.multisend, total, &data, p.safe_nonce, p.chain_id);
    let signature = signer.sign_hash_sync(&hash).expect("local signer");
    let mut signatures = Vec::with_capacity(65);
    signatures.extend_from_slice(&signature.r().to_be_bytes::<32>());
    signatures.extend_from_slice(&signature.s().to_be_bytes::<32>());
    signatures.push(27 + signature.v() as u8);

    let input = execTransactionCall {
        to: p.multisend,
        value: total,
        data: data.into(),
        operation: DELEGATE_CALL,
        safeTxGas: U256::ZERO,
        baseGas: U256::ZERO,
        gasPrice: U256::ZERO,
        gasToken: Address::ZERO,
        refundReceiver: Address::ZERO,
        signatures: signatures.into(),
    }
    .abi_encode();
    let tx = TxEip1559 {
        chain_id: p.chain_id,
        nonce: p.signer_nonce,
        gas_limit: p.gas_limit,
        max_fee_per_gas: p.base_fee,
        max_priority_fee_per_gas: 0,
        to: p.safe.into(),
        value: U256::ZERO,
        access_list: Default::default(),
        input: input.into(),
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).expect("local signer");
    Ok(alloy_consensus::TxEnvelope::from(tx.into_signed(signature)).encoded_2718())
}

/// EIP-712 hash of a Safe delegatecall with zero safeTxGas and no refund.
fn safe_tx_hash(
    safe: Address,
    to: Address,
    value: U256,
    data: &[u8],
    nonce: u64,
    chain: u64,
) -> B256 {
    let domain_type = keccak256("EIP712Domain(uint256 chainId,address verifyingContract)");
    let domain = keccak256((domain_type, U256::from(chain), safe).abi_encode());
    let safe_tx_type = keccak256(
        "SafeTx(address to,uint256 value,bytes data,uint8 operation,uint256 safeTxGas,uint256 baseGas,uint256 gasPrice,address gasToken,address refundReceiver,uint256 nonce)",
    );
    let struct_hash = keccak256(
        (
            safe_tx_type,
            to,
            value,
            keccak256(data),
            U256::from(DELEGATE_CALL),
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            Address::ZERO,
            Address::ZERO,
            U256::from(nonce),
        )
            .abi_encode(),
    );
    let mut preimage = [0u8; 66];
    preimage[..2].copy_from_slice(&[0x19, 0x01]);
    preimage[2..34].copy_from_slice(domain.as_slice());
    preimage[34..].copy_from_slice(struct_hash.as_slice());
    keccak256(preimage)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    fn relay(relay_fee_recipient: Address) -> RelayConfigV1 {
        RelayConfigV1 {
            relay_fee_recipient,
            multisend_contract: Address::ZERO,
            relay_bps: 2500,
            merged_builder_bps: 2500,
            winning_builder_bps: 2500,
            distribution_gas_limit: 140_000,
            builder_collaterals: Vec::new(),
        }
    }

    const ORIGINS: [Address; 2] = [
        address!("0000000000000000000000000000000000000006"),
        address!("0000000000000000000000000000000000000007"),
    ];

    fn sources() -> Vec<Source> {
        ORIGINS.iter().map(|&address| Source { pubkey: Default::default(), address }).collect()
    }

    fn revenues(paid: [u64; 2]) -> Vec<Revenue> {
        (0..2).map(|i| Revenue { source: i as u16, paid: U256::from(paid[i]) }).collect()
    }

    fn get(split: &[(Address, U256)], to: Address) -> Option<U256> {
        split.iter().find(|(a, _)| *a == to).map(|(_, v)| *v)
    }

    #[test]
    fn split_matches_relay_accounting() {
        let (winner, relay_to, proposer) = (
            address!("0000000000000000000000000000000000000001"),
            address!("0000000000000000000000000000000000000002"),
            address!("0000000000000000000000000000000000000003"),
        );
        let revs = revenues([10_000, 30_000]);
        let out = split(&relay(relay_to), &revs, &sources(), U256::ZERO, proposer, winner);
        assert_eq!(out.iter().map(|(_, v)| *v).sum::<U256>(), U256::from(30_000));
        assert_eq!(get(&out, relay_to), Some(U256::from(10_000)));
        assert_eq!(get(&out, proposer), Some(U256::from(10_000)));
        assert_eq!(get(&out, ORIGINS[0]), Some(U256::from(2_500)));
        assert_eq!(get(&out, ORIGINS[1]), Some(U256::from(7_500)));
        assert_eq!(get(&out, winner), None);

        let revs = revenues([7, 5]);
        let out = split(&relay(relay_to), &revs, &sources(), U256::ZERO, proposer, winner);
        assert_eq!(out.iter().map(|(_, v)| *v).sum::<U256>(), U256::from(8));
        assert_eq!(get(&out, proposer), Some(U256::from(3)));
        assert_eq!(get(&out, relay_to), Some(U256::from(3)));
        assert_eq!(get(&out, ORIGINS[0]), Some(U256::from(1)));
        assert_eq!(get(&out, ORIGINS[1]), Some(U256::from(1)));
    }

    #[test]
    fn payment_tx_decodes_as_signed_by_the_relay() {
        let signer: PrivateKeySigner =
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".parse().unwrap();
        let p = SafePayment {
            safe: address!("1111111111111111111111111111111111111111"),
            safe_balance: U256::from(10_000_000u64),
            safe_nonce: 3,
            signer_nonce: 7,
            chain_id: 1,
            gas_limit: 140_000,
            base_fee: 1_000_000_000,
            multisend: address!("2222222222222222222222222222222222222222"),
        };
        let to = [(address!("3333333333333333333333333333333333333333"), U256::from(1_000u64))];
        let encoded = payment_tx(&signer, &p, &to).unwrap();
        let tx = ethrex_common::types::Transaction::decode_canonical(&encoded).unwrap();
        assert_eq!(tx.gas_limit(), 140_000);
        assert_eq!(tx.nonce(), 7);
        let sender = tx.sender(&ethrex_crypto::native::NativeCrypto).unwrap();
        assert_eq!(sender, crate::convert::eaddr(signer.address()));

        let broke = SafePayment { safe_balance: U256::from(999u64), ..p };
        assert_eq!(payment_tx(&signer, &broke, &to), Err("safe_balance"));
    }
}
