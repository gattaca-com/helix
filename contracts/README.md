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

## MultiPaymentForwarder

```
MultiPaymentForwarder 0xFeEeEEEAEA9b3cE71Ce8A252aAa11D2dC439CC69
MultiPaymentLeg       0xFeEeEEE0e9C10f0484111144Ba82788E0B550D91
```

Not deployed yet. Both go through the same CREATE2 proxy as PaymentForwarder
(`script/DeployMultiPayment.s.sol`); the forwarder's runtime hardcodes the leg's
address, so the leg is deployed first.

Pays several recipients in one transaction, bound to one slot like
PaymentForwarder, without running any recipient's code.

```
calldata: [uint32 timestamp][20-byte final recipient]
          then any number of [20-byte recipient][uint96 amount] entries
```

The header is PaymentForwarder's calldata. Each entry's amount is sent to
MultiPaymentLeg, which self-destructs it to the entry's recipient: a
self-destruct always sends the whole balance, so every amount needs its own
account to leave from. Whatever is left afterwards is self-destructed to the
final recipient, so with no entries this pays exactly like PaymentForwarder.

- The call reverts on a timestamp mismatch, if the value does not cover the
  entries, or if the calldata after the header is not whole 32-byte entries.
  Calldata of 24 bytes or less is read like PaymentForwarder's, without a
  length check.
- Anything already sitting in the leg is paid out with the first entry, and
  anything in the forwarder goes to the final recipient.
- Never name the forwarder or the leg as a recipient: an amount paid to either
  just moves on to the final recipient or the next entry.

### Deriving a payment without simulating it

No recipient code runs and nothing is read beyond the calldata, the block
timestamp and the two contracts' balances, so a payer can compute the outcome
instead of simulating it. If the timestamp matches and the value covers the
entries, each entry's recipient gains its amount, the final recipient gains the
value minus the entries plus whatever the forwarder held, the first entry's
recipient also gains whatever the leg held, and both contracts end empty.

Gas follows from the calldata, the access list and two facts about each
payment: whether its recipient was already accessed, and whether its account is
empty when paid. Prague figures, unchanged on Cancun and Osaka:

```
gas  = 21,000 + 4 per zero calldata byte + 16 per non-zero byte
     + 2,400 per access-list address + 1,900 per storage key
     + execution

execution, no entries:        7,669
execution, N >= 1 entries:   10,195 + 14,483 * N
  per payment to a recipient already accessed       -2,600
  per payment of more than 0 to an empty account   +25,000
  per entry with amount 0                           -6,700
  leg in the access list, N >= 1                    -2,500
```

Payments run in execution order: the entries in calldata order, then the final
recipient, although its address comes first in the calldata. A recipient counts
as already accessed if it is the sender, the coinbase, a precompile, in the
access list, or paid earlier in the transaction. It is empty if, when paid, it
has no balance, nonce or code. Listing a recipient that would otherwise be cold
saves 200 net of its access-list entry, and listing the leg saves 100.

A payment is what actually moves: an entry's amount, plus whatever the leg held
for the first entry, and the value left after the entries, plus whatever the
forwarder held, for the final recipient. Anyone can send wei to either contract,
so an entry or a remainder of 0 can still create an account; paying every
recipient more than 0 keeps the gas independent of the contracts' balances.

The EIP-7623 calldata floor never binds, since execution far outweighs it.
EIP-7825 caps a transaction at 2^24 gas: about 1,100 entries to existing
accounts, or 418 if each one creates an account.

A gas limit of exactly this figure is enough. Each call to the leg keeps 1/64
of the remaining gas back, and the final payment after it always leaves more
than that, so no leg runs short.

MultiPaymentLeg's 6 bytes of runtime:

```
5f3560601cff

PUSH0 CALLDATALOAD    // calldata: [20-byte recipient]
PUSH1 0x60 SHR        // recipient
SELFDESTRUCT          // send balance to recipient
```

## Development

Foundry project, not wired into the Rust build. Requires
[forge](https://getfoundry.sh). `foundry.toml` runs each call from a test as a
transaction of its own (`isolate = true`), so the gas tests measure what a
transaction pays. The tests run against the committed runtime, so no Huff
compiler is needed to verify behaviour; `script/check-runtime.sh` checks that
runtime against the MultiPayment Huff sources with
[huff2](https://github.com/huff-language/huff2).

```bash
forge test
forge snapshot    # refresh .gas-snapshot
HUFF=path/to/huff script/check-runtime.sh
```
