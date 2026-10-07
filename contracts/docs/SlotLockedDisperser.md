# SlotLockedDisperser

Multi-recipient generalization of [`PaymentForwarder`](PaymentForwarder.md):
pays N recipients in one call instead of one, for the case where a single
block's extra value needs splitting between several parties (a relay's own
fee, several contributing builders, the proposer) rather than forwarded to
one. Same slot-lock idea, same underlying threat (a payment exposed by a
reorg or a missed slot must not be replayable in a later slot), but `CALL`
in a loop instead of a single `SELFDESTRUCT`, since more than one recipient
needs paying. Not yet deployed.

## Why not just N `PaymentForwarder` calls

Sending N separate slot-locked payments doesn't get you atomicity: if
recipient 3 of 5 can't be paid, 1 and 2 already were, and the tx that would
have reverted the whole batch doesn't exist. `SlotLockedDisperser` pays all
recipients and refunds the caller's leftover in one call, atomically — all
of it happens or none of it does.

## Calldata

```
Offset 0..3:    uint32 timestamp   (must equal block.timestamp)
Offset 4..23:   recipient 1        (20 bytes)
Offset 24..55:  amount 1           (32 bytes)
Offset 56..75:  recipient 2        (20 bytes)
Offset 76..107: amount 2           (32 bytes)
... repeating [20-byte address][32-byte amount] ...
```

No function selector, no length check on the trailing entry — same
calldata-is-caller-constructed assumption as `PaymentForwarder`. Any leftover
balance after all recipients are paid is refunded to `msg.sender`.

## Guarantees

- **Atomic.** Either every recipient is paid and the caller gets the exact
  leftover refund, or the entire call reverts and the contract's balance is
  untouched.
- **Block-locked**, same mechanism as `PaymentForwarder`: reverts with
  `TimestampMismatch()` unless the first 4 bytes of calldata equal
  `block.timestamp`.
- **Reentrancy-guarded.** Unlike `PaymentForwarder`, this contract pays via
  `CALL` (a loop needs to keep going after paying each recipient, which
  `SELFDESTRUCT` doesn't allow), so a malicious recipient's `receive()` could
  otherwise reenter mid-loop — `CALL` forwards all remaining gas — and
  redirect funds meant for other recipients or the refund. A storage-backed
  lock blocks any nested call while one is in flight. Costs ~2.3k gas per
  call (the lock is set then cleared within the same tx, so the EIP-3529
  refund cancels out most of the base `SSTORE` cost).
- **No access control**, same reasoning as `PaymentForwarder`'s "anyone may
  pay": nothing checks `msg.sender`, recipients/amounts are fully
  caller-controlled. Safe only because the contract is never expected to
  hold a balance at rest.

Revert reasons decode as custom errors, not a bare `revert(0,0)`:

```solidity
error AlreadyEntered();                    // reentrancy guard tripped
error TimestampMismatch();                 // calldata timestamp != block.timestamp
error TransferFailed(address recipient);   // named recipient's CALL failed,
                                            // including the final refund-to-caller CALL
```

`TransferFailed` names whichever `CALL` failed first — useful for identifying
a misbehaving recipient after a failed pre-inclusion simulation without a
tracer. Survives most gas-griefing attempts too: EIP-150 only forwards
63/64ths of remaining gas to a `CALL`, so the outer contract keeps a 1/64th
sliver — usually enough to still name the culprit before reverting.

## Known accepted trade-offs

- **Gas griefing.** `CALL` forwards all remaining gas uncapped, so one
  adversarial or merely gas-hungry recipient can force the whole batch to
  revert, blocking payment to every other recipient in the same call.
  Alternatives (capped gas, pull-based claims) trade away either legitimate
  contract-wallet recipients or atomicity; accepted as-is since the caller
  controls calldata construction and can react out-of-band.
- **Misaligned calldata** (trailing entry not an exact 20+32-byte multiple)
  burns gas via an unbounded loop rather than failing fast, since `offset`
  never exactly equals `calldatasize`. Not reachable if the caller
  constructs calldata correctly.

## Formal verification

`test/SlotLockedDisperser.symbolic.t.sol` proves properties with
[Halmos](https://github.com/a16z/halmos) across the full input space rather
than sampling concrete cases: value conservation (exact accounting between
recipients and the caller's refund, for any address/amount/starting balance,
at recipient counts up to the real usage bound), the reentrancy lock
rejecting any nested call regardless of content, a timestamp mismatch never
mutating state, and conservation holding even against a recipient that
always tries to reenter with arbitrary calldata/value. See the test file's
comments for what's proven at which bound and why (mutually-distinct
*symbolic* addresses hit a solver wall around N=5-6; fixing concrete
addresses and leaving amounts symbolic scales the same property to N=25).

## Development

Needs [`huffc`](https://docs.huff.sh/get-started/installing/) — tests deploy
via `HuffDeployer`, which shells out to it (`ffi = true` in `foundry.toml`).
See the top-level README for the Halmos setup/invocation.
