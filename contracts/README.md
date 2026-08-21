# contracts

Slot-locked payment contracts: bind a payment to the one block/slot it was
built for, so a payment exposed by a reorg or a missed slot can't be
replayed into a later slot.

- **[PaymentForwarder](docs/PaymentForwarder.md)** — forwards the contract's
  entire balance to a single recipient. 20 bytes of runtime, deployed at the
  same address on every chain, the canonical payment contract relays
  validate against.
- **[SlotLockedDisperser](docs/SlotLockedDisperser.md)** — pays N recipients
  and refunds the caller's leftover in one atomic call, for splitting a Merged block's added value between several contributors instead of forwarding to one.

## Development

Requires [forge](https://getfoundry.sh). `SlotLockedDisperser` additionally
needs [`huffc`](https://docs.huff.sh/get-started/installing/) (`PaymentForwarder`
tests run against a committed runtime and don't need it).

```bash
forge test
forge snapshot    # refresh .gas-snapshot
```

### Formal verification (Halmos)

`SlotLockedDisperser` can't use the SELFDESTRUCT that PaymentForwarder uses so extra steps have been taken to ensure it's safe to use. It's properties are proven, not sampled, with
[Halmos](https://github.com/a16z/halmos) — see
[docs/SlotLockedDisperser.md](docs/SlotLockedDisperser.md#formal-verification)
for what's proven. Install:

```bash
python3 -m venv .venv-halmos && .venv-halmos/bin/pip install halmos
```

Run (`--ffi` lets `HuffDeployer` shell out to `huffc`, off by default in
Halmos; `--loop` raises the loop-unrolling bound past its default of 2,
enough to cover the largest recipient count checked):

```bash
.venv-halmos/bin/halmos --contract SlotLockedDisperserSymbolicTest --ffi --loop 27
```
