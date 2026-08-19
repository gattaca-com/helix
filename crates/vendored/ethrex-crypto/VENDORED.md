# Vendored ethrex-crypto

Copy of `crates/common/crypto` from
[lambdaclass/ethrex](https://github.com/lambdaclass/ethrex) at rev
`b4d5677812fdf58cb3e439c77906dda4d9b6f91a` (MIT OR Apache-2.0), applied to the
workspace via `[patch]` in the root `Cargo.toml`.

## Why

Upstream's `keccak/` assembly (generated from OpenSSL's `keccak1600-*.pl`)
exports the global symbols `SHA3_absorb`, `SHA3_squeeze` and `KeccakF1600` —
the same names a statically linked (vendored) OpenSSL `libcrypto` defines. In
this workspace, lighthouse's `eth2` crate forces `reqwest/native-tls-vendored`
(static OpenSSL) into every build, so any binary that links both — only
`helix-builder` today — either fails to link with duplicate symbols or, with
`--allow-multiple-definition`, silently mixes the two implementations. The
mix is NOT safe: `KeccakF1600` uses a bespoke internal calling convention that
differs between OpenSSL versions, and cross-resolution corrupts keccak
digests (observed as wrong recovered tx senders).

## Local changes vs upstream

- `keccak/*.s`, `keccak/mod.rs`: global symbols renamed with an `ethrex_`
  prefix (`ethrex_SHA3_absorb`, `ethrex_SHA3_squeeze`, `ethrex_KeccakF1600`,
  `ethrex_SHA3_*_cext`). No functional changes.
- `Cargo.toml`: `workspace = true` inheritances replaced with the concrete
  values from the ethrex workspace; benches and lints dropped.
- `benches/` removed.

## Removal path

Drop this directory and the `[patch]` entry once upstream namespaces the
symbols (worth a PR to ethrex) — or once nothing in this workspace forces a
vendored OpenSSL. When bumping the pinned ethrex rev, diff upstream
`crates/common/crypto` against this copy and re-apply the rename.
