# PaymentForwarder

```
0xFEEEEEECC8AdE925fA6099f017712A04b5546A32
```

Same address on every chain: deployed with the canonical deterministic-deployment
proxy (`0x4e59b44847b379578588920cA78FbF26c0B4956C`) under a mined salt.

## Why

A proposer payment sent as a plain transfer is valid in any block. If the block
it was built for is reorged out, or the slot is missed once the payload is
public, the signed transfer can be replayed in a later slot and the payer pays
the previous slot's fee recipient a second time.

Paying through this contract binds the payment to one slot. The recipient and the
expected `block.timestamp` travel in the calldata, and the call reverts if the
timestamp does not match, so a replay in any later slot fails.

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

with the value equal to the bid in both cases. The contract forwards the full
value with all remaining gas and reverts if that call fails, so a successful
receipt for either shape means the fee recipient was paid in full.

## Bid adjustments

An adjustment rewrites the payment from the relay's own fee payer. Because the
contract does not authenticate the sender, it can keep every field of the
original payment except the nonce and the value.

Keeping `to` and the calldata is what an implementation must do: the substituted
transaction then costs exactly the gas the builder reported, so the header
`gas_used`, the payment receipt's `cumulative_gas_used` and the fee payer's
balance all stay consistent without re-estimating anything. Replacing a forwarded
payment with a direct transfer changes its gas and invalidates the block.

## Development

Foundry project, not wired into the Rust build. Requires
[forge](https://getfoundry.sh).

```bash
forge test        # includes fuzz and gas-invariance properties
forge snapshot    # refresh .gas-snapshot after changing the source
```

`foundry.toml` pins the compiler and strips metadata so the build is
reproducible, and `test_runtimeBytecodeMatchesCommitted` checks the compiled
runtime against the deployed bytecode. Any change to the source or the compiler
settings changes that bytecode and the address, and is a new deployment.

`withdraw()` sends the balance to a single address fixed in the runtime. A
payment never leaves value in the contract, so it is unreachable in normal
operation and exists only to recover funds sent to the address by mistake.
