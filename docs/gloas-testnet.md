# Running helix on a Gloas testnet

How to exercise the relay's whole path — submit, simulate, `get_header`,
`get_payload`, publish — on a Gloas consensus layer with an Amsterdam execution
layer, using the ethrex-based `helix-builder` for both the builder and the
simulator.

Three roles take part. Block merging does not: see
[What is not supported](#what-is-not-supported).

| component | what runs it |
| --- | --- |
| relay | `helix-relay` |
| simulator | `helix-builder --sim.config` |
| builder | `helix-builder --build.config` |

## What Gloas changes

Two forks arrive together and both matter here.

**Gloas (consensus, EIP-7732)** moves the execution payload into a separately
bid and revealed envelope. helix's builder-facing wire shape is unchanged except
for one addition: a Gloas submission carries the EIP-7928 block access list
beside `blobs_bundle` and `execution_requests`.

**Amsterdam (execution)** adds two header fields no `ExecutionPayloadV3` carries:

- `block_access_list_hash` (EIP-7928), the keccak of the encoded access list
- `slot_number` (EIP-7843), the proposal slot

The block hash commits to both, so the simulator has to reconstruct them from
the submission. The list arrives as opaque bytes and is **hashed exactly as
received, never re-encoded** — re-encoding could change the hash and break the
commitment the builder signed. The slot number comes from `BidTrace.slot`, so
the bid trace and the header must name the same slot.

The builder computes the list while building and the simulator recomputes it
during validation, so a list that does not describe execution is rejected rather
than trusted.

**The two forks are selected independently and must agree.** The block's shape
follows the execution layer (`amsterdamTime` in the EL genesis) and the
submission's shape follows the consensus layer (the Gloas fork epoch in the CL
config). A submission whose shape cannot carry its block's list is refused
before it is sent, because sending it would drop the list and every bid would
die as an unexplained block hash mismatch.

## Execution layer genesis

### The EIP-8282 predeploys are mandatory

Amsterdam runs two system contracts — the EIP-8282 builder deposit and builder
exit predeploys. **Empty code at either address invalidates every Amsterdam
block**, with `SystemContractCallFailed: ... has no code after deployment`. Both
must be allocated in genesis:

| address | contract |
| --- | --- |
| `0x0000884d2AA32eAa155F59A2f24eFa73D9008282` | builder deposit |
| `0x000014574A74c805590AFF9499fc7A690f008282` | builder exit |

Their runtime bytecode is in ethrex's own `fixtures/genesis/l1-bal.json`, and in
this repo in `crates/builder/src/testing.rs`
(`deploy_amsterdam_predeploys`), which is what the Amsterdam test fixtures use.

### Fork activation

```json
{
  "config": {
    "shanghaiTime": 0,
    "cancunTime": 0,
    "pragueTime": 0,
    "osakaTime": 0,
    "amsterdamTime": 0
  }
}
```

An explicit `blobSchedule.amsterdam` entry is optional: with none, ethrex falls
through the BPO chain to Osaka's schedule.

## Consensus layer

Set the Gloas fork epoch in the beacon chain config. The builder reads the spec
and genesis from the beacon node (`get_chain_info`) and derives both the builder
signing domain and the fork at each slot from it, so there is no compiled-in
fork version to keep in step — but the beacon node's spec must actually schedule
Gloas, or the builder will send pre-Gloas submissions for Amsterdam blocks and
refuse them itself.

The relay picks the decode fork from `ChainInfo::current_fork_name()`, i.e. from
the same spec by wall clock. Relay and builder must read the same beacon node,
or the same config.

## Relay configuration

### Every simulator needs `ssz_url`

The ethrex simulation role serves the SSZ routes (`/validate`,
`/validate_merged`) and no JSON-RPC validation method. Simulator dispatch is
per-simulator: a simulator entry without `ssz_url` takes the JSON-RPC path, and
`sim_request_builder` returns `None` for Gloas, so **that simulator silently
drops every Gloas submission** with `BlockSimError::UnsupportedFork`.

Any reth-based simulator in the pool has the same effect, because reth has no
Amsterdam support yet (see #518). For a Gloas testnet, every `simulators` entry
must point at an ethrex simulation role.

```yaml
simulators:
  # `url` is still required: the relay uses it for eth_syncing and
  # eth_getBalance. Point it at the same node's ethrex JSON-RPC
  # (--http.addr/--http.port, default 127.0.0.1:8545).
  - url: http://127.0.0.1:8545
    ssz_url: http://127.0.0.1:8552
```

`ssz_url` is the simulation role's `ssz_addr`.

### Merging off

`block_merging_config.is_enabled` defaults to `false`; leave it there. With it
on, the merging role declines every Amsterdam base block by name and the merge
tile does no useful work.

### The builder must be registered

Add the building role's BLS pubkey to `builders` with an `api_key`, which the
builder sends as `x-api-key`.

## Running the simulator and builder

Both roles share one embedded node, so they can run in one process:

```bash
helix-builder --sim.config sim-config.yml --build.config build-config.yml \
  --http.port 8545 --authrpc.port 8551
```

The node needs a beacon node driving its Engine API on `--authrpc.port`, exactly
as any execution client does.

**Release builds only for these two roles.** A blob is 128 KiB and crosses the
stack several times in a debug build, which overflows tokio's default worker
stack.

### Keys

| variable | needed by |
| --- | --- |
| `BUILDER_BLS_KEY` | building — signs the bid, and is the pubkey to register |
| `BUILDER_PAYOUT_KEY` | building — secp256k1, funds the proposer payment |
| `RELAY_KEY` | merging only, so not needed here |

The payout key's account must hold enough to cover the bid plus the payout's own
gas. Both keys are loaded at startup, before the node boots, so a bad key is a
startup error rather than a first-slot failure.

### `payout_gas_reserve` under Amsterdam

The building role ends each block with a plain transfer to the proposer's
registered fee recipient:

```
payout = tips + subsidy_wei - payout_gas * base_fee
```

`payout_gas_reserve` (default 21000) covers that transfer. **Paying an address
for the first time also creates its account, and EIP-8037 charges state gas for
that** — 120 state bytes at 1530 gas each, so 183600 on top, for a total of
204600. A fee recipient that has never been paid is the normal case on a fresh
testnet.

The builder handles this: it reads the recipient from in-block state and adds
the creation charge only when the account is absent. `payout_gas_reserve` should
stay at 21000 unless the fee recipient is a contract that needs more for the
transfer itself. A contract needing more than the reserve makes the payout fail
and the slot is skipped.

`subsidy_wei` (default 1e15) exists because the relay rejects a zero-value
block. It must exceed the payout's own gas cost or every idle-mempool block is
skipped with `NoPayout`; at 204600 gas the default covers base fees up to
roughly 4.9 gwei. Set it to 0 only if you want bids solely from real tips.

## Generating traffic

**A flat 21000-gas transfer to a fresh address fails under Amsterdam.** This is
EIP-8037 applying to all traffic, not something helix can change: the intrinsic
cost of the transfer is still 21000, but creating the recipient adds 183600 in
state gas. Such transactions are included in blocks with failed receipts, which
is correct behaviour and looks like a broken builder if you are not expecting
it.

Funding scripts and traffic generators aimed at a Gloas testnet need a gas limit
above 204600 for any transfer to an address that does not exist yet.

## Verifying it works

1. **The simulator accepts a block.** The relay logs a successful simulation, or
   the simulation role returns `200` on `/validate`. A `501` means the fork gate
   refused the submission: the relay sent a fork this role does not serve.
2. **The relay accepts the submission.** `200 OK` on
   `/relay/v1/builder/blocks`. A `400` carries the reason in its body, which is
   how an operator learns the blocks are bad.
3. **The bid is visible** through the relay's data API.
4. **`get_header` returns the bid** for that slot.
5. **`get_payload` and publication complete**, which is the point of the whole
   exercise.

If step 1 fails with a block hash mismatch, suspect the genesis: a missing
Amsterdam predeploy, or an `amsterdamTime` that disagrees with the CL's Gloas
epoch.

## What is not supported

- **Block merging.** Merging protocol v1 carries an `ExecutionPayloadV3`, which
  has nowhere to put the access list or slot number, so the merging role
  declines Amsterdam bases by name and the merged validation route refuses
  Gloas. Tracked in #576. This is a missing capability, not a broken one: the
  role refuses rather than merging into a block whose hash it cannot reproduce.
- **The reth simulator.** No Amsterdam support; tracked in #518.
- **Bid adjustments on Gloas, and dehydrated or mergeable Gloas submissions.**
  Each is refused explicitly rather than decoded into a Fulu shape that would
  drop the access list.
- **Inclusion lists** in the ethrex simulation role. A block violating a
  submitted list passes here and fails in `crates/simulator`.

## Reference: the access list's size

Measured against ethrex's encoding at the pinned revision, since it decides
whether the builder sending the list is affordable. Per item, counted the way
EIP-7928's cap counts:

| item | bytes |
| --- | --- |
| account (address, balance and nonce change) | ~70 |
| storage read (slot only) | ~33 |
| storage change (slot, value, index) | ~71 |

A realistic busy block is small: 400 transfers to 400 distinct recipients
produced a 29.7 KB list. The worst case is bounded by EIP-7928 itself, which
caps items at `gas_limit / 2000` — roughly 0.4–0.9 MB at a 25M gas limit,
0.7–1.6 MB at 45M and 1.0–2.1 MB at 60M, spanning an all-reads and an
all-changes list.

Even that maximum is an order of magnitude inside the relay's 20 MB
`MAX_PAYLOAD_LENGTH`, and far inside `BlockAccessListBytes`'s own SSZ bound of
1 GB. This is why the builder sends the list rather than the simulator returning
it: the bandwidth is affordable, and a list the builder committed to is worth
more than one nobody signed.
