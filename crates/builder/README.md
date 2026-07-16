# helix-builder

An external block-merging builder: the TCP **server** counterpart of the
relay's block-merging tile (`crates/relay/src/block_merging/`), with an
embedded [ethrex](https://github.com/lambdaclass/ethrex) execution node
providing chain state and the EVM.

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

## Architecture

```
tokio runtime    embedded ethrex node: store (rocksdb), devp2p + snap sync,
                 Engine API (authrpc) for the operator's beacon node, head watcher
flux tile        merging TCP server (listen, handshake, framing, routing)
engine thread    merge worker: order pool, base replay, presim (rayon), emission
```

The tile and engine communicate over bounded crossbeam channels; the engine
owns all merge state and never blocks the TCP thread.

## Running

The builder is a full execution node and needs a **beacon node** driving its
Engine API to follow the chain:

```sh
RELAY_KEY=0x... helix-builder \
  --network mainnet \
  --datadir /data/helix-builder \
  --authrpc.addr 0.0.0.0 --authrpc.jwtsecret /secrets/jwt.hex \
  --merging.config merging.yml
```

- Node flags mirror the upstream `ethrex` binary (same names and `ETHREX_*`
  env vars). `--datadir memory --p2p.disabled` boots an ephemeral in-memory
  node for local testing.
- `RELAY_KEY` holds the private key of the Safe owner that signs the revenue
  distribution transaction (the same key the relay-side simulator uses).
- `--merging.config` points at the builder-owned YAML section; see
  [config.example.yml](config.example.yml). The `api_keys` allowlist must
  contain the key the relay presents in `MergerRegistrationV1`.

On the relay side, add the builder to `block_merging_config.tcp.builders`
(see the repo-root `config.example.yml`).

## Limitations

- Merging protocol v1 carries `ExecutionPayloadV3`; post-Amsterdam blocks
  (EIP-7928 block access lists) are rejected as merge bases.
- The base block's declared `block_hash` is trusted as the pool key; the wire
  format carries no `requests_hash` to fully recompute it.
- P-256 (`P256VERIFY`) uses ethrex's portable fallback rather than the
  aws-lc-rs backend (its cc requirement conflicts with reth's pin in this
  workspace); this only affects that precompile's throughput.
