use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, U256, U512, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, SolValue, sol};
use helix_tcp_types::merging::control::RelayConfigV1;

use crate::merge::pool::Submitter;

const TOTAL_BPS: u64 = 10_000;
const DELEGATE_CALL: u8 = 1;

pub struct Revenue {
    pub submitter: u16,
    pub paid: U256,
}

pub fn split_revenue(
    relay: &RelayConfigV1,
    revenues: &[Revenue],
    submitters: &[Submitter],
    cost: U256,
    gas_payer: Address,
    proposer: Address,
    beneficiary: Address,
) -> Vec<(Address, U256)> {
    let total: U256 = revenues.iter().map(|r| r.paid).sum();
    let net = total - cost;
    let share = |bps: u64, value: U256| U256::from(bps) * value / U256::from(TOTAL_BPS);
    let proposer_bps =
        TOTAL_BPS - relay.relay_bps - relay.merged_builder_bps - relay.winning_builder_bps;

    let mut out: Vec<(Address, U256)> = Vec::with_capacity(revenues.len() + 3);
    let mut credit = |to: Address, value: U256| match out.iter_mut().find(|(a, _)| *a == to) {
        Some((_, v)) => *v += value,
        None => out.push((to, value)),
    };
    credit(proposer, share(proposer_bps, net));
    credit(relay.relay_fee_recipient, share(relay.relay_bps, net));
    for revenue in revenues {
        let actual = (revenue.paid.widening_mul(net) / U512::from(total)).to::<U256>();
        let origin = submitters[revenue.submitter as usize].address;
        credit(origin, share(relay.merged_builder_bps, actual));
    }
    if !cost.is_zero() {
        credit(gas_payer, cost);
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
    // Zero safeTxGas forwards all gas and reverts the Safe call if multiSend fails.
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
