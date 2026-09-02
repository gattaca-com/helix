# helix-builder

An embedded [ethrex](https://github.com/lambdaclass/ethrex) execution node
running one or both of two roles, selected by which config file is supplied:

| role | config | what it does |
| --- | --- | --- |
| merging | `--merging.config` | external block-merging builder: the TCP server counterpart of the relay's block-merging tile (`crates/relay/src/block_merging/`) |
| simulation | `--sim.config` | block validator: the ethrex counterpart of `crates/simulator`, serving the relay's SSZ validation routes |

Supplying neither is a startup error. Both share one node, and `RELAY_KEY` is
only needed for the merging role.

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

## Architecture

```
tokio runtime    embedded ethrex node: store (rocksdb), devp2p + snap sync,
                 Engine API (authrpc) for the operator's beacon node, head watcher
                 simulation role: SSZ validation server, disallow-list refresh
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
  the simulation role in release.
- The base block's declared `block_hash` is trusted as the pool key; the wire
  format carries no `requests_hash` to fully recompute it.
- P-256 (`P256VERIFY`) uses ethrex's portable fallback rather than the
  aws-lc-rs backend (its cc requirement conflicts with reth's pin in this
  workspace); this only affects that precompile's throughput.
