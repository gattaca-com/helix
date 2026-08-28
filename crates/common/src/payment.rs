use alloy_primitives::{Address, U256};
use alloy_sol_types::{SolCall, sol};

sol! {
    /// Gnosis Safe entry point the merge-builder payment engine calls
    /// (crates/builder/src/engine/payment.rs) to delegatecall `multiSend`.
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

    /// Gnosis `MultiSendCallOnly` entry point, delegatecalled by `execTransaction`.
    function multiSend(bytes transactions) external payable;
}

pub const SAFE_DELEGATECALL: u8 = 1;

/// Sums the value a Safe `execTransaction` -> `multiSend` delegatecall's batched
/// calls pay `recipient` (zero on any decode failure or shape mismatch: not a
/// delegatecall, not a multiSend payload, no matching entry). Best-effort
/// recognition, not a signature/authenticity check: the caller must already know
/// the transaction actually succeeded on-chain.
pub fn multisend_paid_amount(input: &[u8], recipient: Address) -> U256 {
    let Ok(exec) = execTransactionCall::abi_decode(input) else { return U256::ZERO };
    if exec.operation != SAFE_DELEGATECALL {
        return U256::ZERO;
    }
    let Ok(multisend) = multiSendCall::abi_decode(&exec.data) else { return U256::ZERO };
    multisend_entries(&multisend.transactions)
        .filter(|(to, _)| *to == recipient)
        .fold(U256::ZERO, |acc, (_, value)| acc + value)
}

/// Iterates a Safe `multiSend` packed payload: per entry, `[1B operation][20B
/// to][32B value][32B dataLength][dataLength B data]`. Stops (yields no more
/// entries) on any length that doesn't fit the remaining bytes, rather than
/// panicking on malformed/adversarial input.
pub fn multisend_entries(transactions: &[u8]) -> impl Iterator<Item = (Address, U256)> + '_ {
    const ENTRY_HEADER_LEN: usize = 1 + 20 + 32 + 32;
    let mut offset = 0usize;
    std::iter::from_fn(move || {
        if transactions.len().saturating_sub(offset) < ENTRY_HEADER_LEN {
            return None;
        }
        offset += 1; // operation: irrelevant for a payment check
        let to = Address::from_slice(&transactions[offset..offset + 20]);
        offset += 20;
        let value = U256::from_be_slice(&transactions[offset..offset + 32]);
        offset += 32;
        let data_len: usize =
            U256::from_be_slice(&transactions[offset..offset + 32]).try_into().ok()?;
        offset += 32;
        if transactions.len().saturating_sub(offset) < data_len {
            return None;
        }
        offset += data_len;
        Some((to, value))
    })
}

#[cfg(test)]
mod multisend_payment_tests {
    use alloy_primitives::{Bytes, address};
    use alloy_sol_types::SolCall;

    use super::*;

    /// Mirrors `crates/builder/src/engine/payment.rs::build_multisend_payload`.
    fn multisend_payload(entries: &[(Address, U256)]) -> Vec<u8> {
        let mut payload = Vec::new();
        for (to, value) in entries {
            payload.push(0u8); // operation = CALL
            payload.extend_from_slice(to.as_slice());
            payload.extend_from_slice(&value.to_be_bytes::<32>());
            payload.extend_from_slice(&U256::ZERO.to_be_bytes::<32>()); // data length
        }
        payload
    }

    /// Mirrors `crates/builder/src/engine/payment.rs::encode_multisend_calldata`'s
    /// `execTransaction` wrapping, minus the real Safe signature (irrelevant here:
    /// `multisend_paid_amount` only decodes calldata shape, it doesn't verify
    /// signatures).
    fn exec_transaction_calldata(
        multisend_contract: Address,
        entries: &[(Address, U256)],
    ) -> Vec<u8> {
        let multisend_calldata =
            multiSendCall { transactions: multisend_payload(entries).into() }.abi_encode();
        execTransactionCall {
            to: multisend_contract,
            value: U256::ZERO,
            data: multisend_calldata.into(),
            operation: SAFE_DELEGATECALL,
            safeTxGas: U256::ZERO,
            baseGas: U256::ZERO,
            gasPrice: U256::ZERO,
            gasToken: Address::ZERO,
            refundReceiver: Address::ZERO,
            signatures: Bytes::new(),
        }
        .abi_encode()
    }

    #[test]
    fn multisend_paid_amount_finds_matching_entry_among_several() {
        let multisend_contract = address!("0x1111111111111111111111111111111111111111");
        let proposer = address!("0x2222222222222222222222222222222222222222");
        let other = address!("0x3333333333333333333333333333333333333333");
        let calldata = exec_transaction_calldata(multisend_contract, &[
            (other, U256::from(100)),
            (proposer, U256::from(42)),
        ]);

        assert_eq!(multisend_paid_amount(&calldata, proposer), U256::from(42));
    }

    #[test]
    fn multisend_paid_amount_sums_multiple_entries_to_the_same_recipient() {
        let multisend_contract = address!("0x1111111111111111111111111111111111111111");
        let proposer = address!("0x2222222222222222222222222222222222222222");
        let calldata = exec_transaction_calldata(multisend_contract, &[
            (proposer, U256::from(42)),
            (proposer, U256::from(8)),
        ]);

        assert_eq!(multisend_paid_amount(&calldata, proposer), U256::from(50));
    }

    #[test]
    fn multisend_paid_amount_zero_when_recipient_absent() {
        let multisend_contract = address!("0x1111111111111111111111111111111111111111");
        let proposer = address!("0x2222222222222222222222222222222222222222");
        let other = address!("0x3333333333333333333333333333333333333333");
        let calldata = exec_transaction_calldata(multisend_contract, &[(other, U256::from(42))]);

        assert_eq!(multisend_paid_amount(&calldata, proposer), U256::ZERO);
    }

    #[test]
    fn multisend_paid_amount_zero_for_non_delegatecall_operation() {
        let multisend_contract = address!("0x1111111111111111111111111111111111111111");
        let proposer = address!("0x2222222222222222222222222222222222222222");
        let multisend_calldata =
            multiSendCall { transactions: multisend_payload(&[(proposer, U256::from(42))]).into() }
                .abi_encode();
        let calldata = execTransactionCall {
            to: multisend_contract,
            value: U256::ZERO,
            data: multisend_calldata.into(),
            operation: 0, // CALL, not DELEGATECALL
            safeTxGas: U256::ZERO,
            baseGas: U256::ZERO,
            gasPrice: U256::ZERO,
            gasToken: Address::ZERO,
            refundReceiver: Address::ZERO,
            signatures: Bytes::new(),
        }
        .abi_encode();

        assert_eq!(multisend_paid_amount(&calldata, proposer), U256::ZERO);
    }

    #[test]
    fn multisend_paid_amount_zero_for_unrelated_calldata() {
        let proposer = address!("0x2222222222222222222222222222222222222222");
        assert_eq!(multisend_paid_amount(&[0xde, 0xad, 0xbe, 0xef], proposer), U256::ZERO);
    }

    #[test]
    fn multisend_entries_stops_on_truncated_payload() {
        let to = address!("0x2222222222222222222222222222222222222222");
        let mut payload = multisend_payload(&[(to, U256::from(42))]);
        payload.truncate(payload.len() - 1); // cut into the last entry's data-length field

        assert_eq!(multisend_entries(&payload).count(), 0);
    }
}
