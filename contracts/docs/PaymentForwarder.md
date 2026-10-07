# PaymentForwarder

```
0xFEEEEEE44046c3f61a8CC081E0918eF0de0a7ffC
```

Same address on every chain: deployed with the canonical deterministic-deployment
proxy (`0x4e59b44847b379578588920cA78FbF26c0B4956C`) under a mined salt.

## Why

A proposer payment sent as a plain transfer is valid in any block. If the block
it was built for is reorged out, or the slot is missed once the payload is
public, the signed transfer can be replayed in a later slot and the payer pays
the previous slot's fee recipient a second time.

Paying through this contract binds the payment to one slot. The recipient and the
expected timestamp travel in the calldata, and the call reverts unless
`block.timestamp` matches.

## The contract

20 bytes of runtime, in full:

```
5f358060e01c4218600f5760401cff5b5f5ffd00

PUSH0 CALLDATALOAD    // calldata word: [uint32 timestamp][20-byte recipient]
DUP1 PUSH1 0xe0 SHR   // timestamp
TIMESTAMP XOR         // zero if it matches
PUSH1 0x0f JUMPI      // otherwise revert
PUSH1 0x40 SHR        // recipient
SELFDESTRUCT          // send balance to recipient
JUMPDEST PUSH0 PUSH0 REVERT
```

Since EIP-6780 `SELFDESTRUCT` just sends the balance to the recipient. A payment
costs at most 29,022 gas, or 54,022 when the recipient account does not exist
yet, and does not depend on the recipient's code; zero bytes in the calldata come
in a few gas under. That figure is also the minimum viable gas limit, since
nothing is held in flight. The recipient is not executed, so nothing can be left
in the contract and a recipient that would reject a transfer is still paid.

Calldata is not forwarded and there is no length check: a payment that omits the
recipient sends the balance to the zero address, so callers must encode all 24
bytes.

## Payment verification

This is the canonical payment contract for builders, so a relay validating a
submission has to accept it alongside a direct transfer.

`ensure_payment` prefers the fee recipient's balance delta, which is unaffected
by the forwarder. When that check does not cover the bid, the fallback inspects
the last transaction, and a forwarded payment does not look like a transfer: `to`
is the forwarder and the calldata is not empty. The fallback therefore accepts
either shape:

- `to` is the fee recipient with empty calldata, or
- `to` is `PAYMENT_FORWARDER` and the recipient in its calldata is the fee
  recipient

with the value equal to the bid in both cases. A successful receipt for the
second shape means the balance reached the recipient, since the contract has no
other path to success.

## Bid adjustments

An adjustment rewrites the payment from the relay's own fee payer. The contract
does not authenticate the sender, so it can keep every field of the original
payment except the nonce and the value.

Keeping `to` and the calldata is what an implementation must do: the substituted
transaction then costs exactly the gas the builder reported, so the header
`gas_used`, the payment receipt's `cumulative_gas_used` and the fee payer's
balance all stay consistent without re-estimating anything. Replacing a forwarded
payment with a direct transfer changes its gas and invalidates the block.

## Development

Unlike `SlotLockedDisperser`, tests run against the committed runtime
(`hex"..."`, hardcoded in `test/PaymentForwarder.t.sol`), so no Huff compiler
is needed to verify behaviour — see the top-level README for the general
`forge test` / `forge snapshot` workflow.
