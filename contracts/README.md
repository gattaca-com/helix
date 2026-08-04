# Contracts

Foundry project, not wired into the Rust build. Requires [forge](https://getfoundry.sh).

```bash
forge test        # includes fuzz and gas-invariance properties
forge snapshot    # refresh .gas-snapshot after changing the source
```

## PaymentForwarder

A proposer payment sent as a plain transfer stays valid in any later block, so
one exposed by a reorg or a missed slot can be replayed once the slot it was
built for has passed. Paying through this contract binds the payment to a single
slot: the target and the expected `block.timestamp` are in the calldata, and the
call reverts if the timestamp does not match.

Deployed with the canonical deterministic-deployment proxy
(`0x4e59b44847b379578588920cA78FbF26c0B4956C`) under a mined salt, so the address
is the same on every chain:

```
0xFEEEEEECC8AdE925fA6099f017712A04b5546A32
```

For a relay validating a submission, the payment is still a balance change on the
fee recipient within the last transaction, but `to` is the forwarder rather than
the fee recipient and the calldata is non-empty. Value is forwarded with all
remaining gas and a failed call reverts, so the fee recipient either receives the
full value or the block does not contain a payment.

Because the sender is not authenticated, a payment can be re-signed by a
different sender while keeping the same target and calldata, which leaves its gas
cost unchanged. `withdraw()` is restricted to the address baked into the runtime;
a payment never leaves value in the contract, so it is only reachable for funds
sent here by mistake.

`foundry.toml` pins the compiler and strips metadata, so the build is
reproducible: `test_runtimeBytecodeMatchesCommitted` checks the compiled runtime
against the deployed bytecode. Any change to the source or the compiler settings
changes that bytecode and the CREATE2 address, and is a new deployment.
