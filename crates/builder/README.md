# helix-builder

An embedded [ethrex](https://github.com/lambdaclass/ethrex) execution node
running any combination of three roles, selected by which config files are
supplied:

| role | config | what it does |
| --- | --- | --- |
| merging | `--merging.config` | external block-merging builder: the TCP server counterpart of the relay's block-merging tile (`crates/relay/src/block_merging/`) |
| simulation | `--sim.config` | block validator: the ethrex counterpart of `crates/simulator`, serving the relay's SSZ validation routes |
| building | `--build.config` | block builder: fills a block from the node's mempool and submits it to the relay |

Supplying none is a startup error. The roles share one node. `RELAY_KEY` is
needed only for merging, and `BUILDER_BLS_KEY` / `BUILDER_PAYOUT_KEY` only for
building.

## The merging role

The TCP **server** counterpart of the relay's block-merging tile, with the
embedded node providing chain state and the EVM.

The relay dials the builder and streams, per slot, the mergeable builder
submissions it receives plus an activation for its current top bid. The
builder replays the activated base block on top of its synced head, greedily
appends profitable orders drawn from the other submissions, appends a Safe
`multiSend` transaction distributing the merged revenue (proposer / relay /
origin builders, per the relay-supplied bps), and streams improved
`MergedBlockV1`s back. Protocol wire types live in
`crates/tcp-types/src/merging/`; the merge algorithm is a port of the
reth-based engine in `crates/simulator/src/block_merging/` onto ethrex's
payload-building primitives.

## The simulation role

Validates a submitted block against the node's own state: the payload converts
to an ethrex block, the bid trace must describe that block, the parent must be
within the validation window, then execution, the post-execution roots and
state root, the blobs bundle's KZG proofs, the disallow list and the proposer
payment. Nothing is written to the store.

It serves the relay's SSZ routes only, `/validate` and `/validate_merged`, so
the relay must reach it through the simulator's `ssz_url`. Differences from
`crates/simulator` worth knowing before pointing traffic at it:

- Inclusion lists are not enforced. A block that violates a submitted list
  passes here and fails there.
- The disallow list rejects interaction by effect -- a state change, or a
  transaction addressed to a listed account. `crates/simulator` also rejects a
  block that merely *reads* one, so it rejects strictly more blocks.
- Only Fulu (V5) and the relay-internal merged method are served.

## The building role

Builds a block for the next slot and submits it to the relay. It is meant for
testnets: it lets the relay's whole path -- submit, simulate, `get_header`,
`get_payload`, publish -- be exercised with a builder you control.

Two sources are merged into one slot context, because neither is sufficient
alone. The beacon node's `payload_attributes` SSE topic gives the parent,
timestamp, `prev_randao`, withdrawals and `parent_beacon_block_root`; the
relay's `get_validators` gives the proposer's pubkey, fee recipient and
registered gas limit. A slot whose proposer has not registered is skipped.

The block itself is ethrex's own payload machinery, with the builder as the
coinbase, so tips accrue to the builder and the bid is funded from them:

```
payout = tips + subsidy_wei - payout_gas_reserve * base_fee
```

The block ends with a plain transfer of `payout` to the proposer's registered
fee recipient. `subsidy_wei` exists because the relay rejects a zero-value
block: without it an idle testnet would produce no bids at all, which is when
the builder is least useful. Set it to 0 to bid only what the block earns.

Gas for that transfer is held back from the fill by lowering ethrex's
`remaining_gas` before `fill_transactions` and restoring it afterwards.

The builder signs under the domain read from the beacon node's own spec and
genesis, never a compiled-in fork version. It builds at each
`submit_offsets_ms` point in the slot and submits only when the value beats
what it already sent for that slot and parent -- a new parent, after a re-org,
starts a fresh auction.

What it does not do: no bundles or `eth_sendBundle`, no ordering of its own
(ethrex's tip-sorted fill), no cancellations, no bidding strategy (it always
bids the full block value), and one relay only.

## Architecture

```
tokio runtime    embedded ethrex node: store (rocksdb), devp2p + snap sync,
                 Engine API (authrpc) for the operator's beacon node, head watcher
                 simulation role: SSZ validation server, disallow-list refresh
                 building role: payload_attributes SSE, duty poll, build + submit
flux tile        merging TCP server (listen, handshake, framing, routing)
engine thread    merge worker: order pool, base replay, presim (rayon), emission
```

The tile and engine communicate over bounded crossbeam channels; the engine
owns all merge state and never blocks the TCP thread. Validation runs on the
tokio blocking pool, capped by `max_concurrent_validations`.

## Running

Either role is a full execution node and needs a **beacon node** driving its
Engine API to follow the chain.

Merging:

```sh
RELAY_KEY=0x... helix-builder \
  --network mainnet \
  --datadir /data/helix-builder \
  --authrpc.addr 0.0.0.0 --authrpc.jwtsecret /secrets/jwt.hex \
  --merging.config merging.yml
```

Simulation:

```sh
helix-builder \
  --network mainnet \
  --datadir /data/helix-sim \
  --authrpc.addr 0.0.0.0 --authrpc.jwtsecret /secrets/jwt.hex \
  --sim.config sim.yml
```

- Node flags mirror the upstream `ethrex` binary (same names and `ETHREX_*`
  env vars). `--datadir memory --p2p.disabled` boots an ephemeral in-memory
  node for local testing.
- `RELAY_KEY` holds the private key of the Safe owner that signs the revenue
  distribution transaction (the same key the relay-side simulator uses).
- `--merging.config` points at the builder-owned YAML section; see
  [config.example.yml](config.example.yml). The `api_keys` allowlist must
  contain the key the relay presents in `MergerRegistrationV1`.

- `--sim.config` points at the simulation YAML; see
  [sim-config.example.yml](sim-config.example.yml). Until
  `blacklist_endpoint` answers, the list is empty and no block is filtered.

Building:

```sh
BUILDER_BLS_KEY=0x... BUILDER_PAYOUT_KEY=0x... helix-builder \
  --network hoodi \
  --datadir /data/helix-builder \
  --authrpc.addr 0.0.0.0 --authrpc.jwtsecret /secrets/jwt.hex \
  --build.config build.yml
```

- `--build.config` points at the building YAML; see
  [build-config.example.yml](build-config.example.yml).
- `BUILDER_BLS_KEY` signs the submission. Register its pubkey with the relay,
  under `builders`, with the `api_key` the config sends.
- `BUILDER_PAYOUT_KEY` signs the payment to the proposer and **must be
  funded**: every bid is paid from this account. Both the pubkey and the
  address are logged at startup.
- These are separate from `RELAY_KEY`, which the merging role reads as a
  secp256k1 key and `helix-common` reads as a BLS one.

On the relay side, add a merging builder to
`block_merging_config.tcp.builders`, and a simulator as a `simulators` entry
with `ssz_url` set to this role's `ssz_addr` (see the repo-root
`config.example.yml`).

## Limitations

- Merging protocol v1 carries `ExecutionPayloadV3`; post-Amsterdam blocks
  (EIP-7928 block access lists) are rejected as merge bases. The simulation
  role has the same gap: the payload carries no block-access-list hash, so
  Amsterdam needs a newer payload version.
- The simulation role serves no JSON-RPC, so a relay without `ssz_url` cannot
  use it.
- A blob is 128 KiB and crosses the stack several times in a debug build, which
  overflows tokio's default worker stack. Release builds elide the copies; run
  the simulation and building roles in release.
- The building role's payout uses a fixed `payout_gas_reserve`, 21000 by
  default. A contract fee recipient needing more makes the payout fail and the
  slot is skipped.
- The building role includes a blob transaction only when its sidecar reached
  the node's mempool over devp2p; it does not rebuild sidecars.
- The base block's declared `block_hash` is trusted as the pool key; the wire
  format carries no `requests_hash` to fully recompute it.
- P-256 (`P256VERIFY`) uses ethrex's portable fallback rather than the
  aws-lc-rs backend (its cc requirement conflicts with reth's pin in this
  workspace); this only affects that precompile's throughput.
