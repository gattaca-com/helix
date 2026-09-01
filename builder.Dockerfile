FROM lukemathwalker/cargo-chef:latest-rust-1.96 AS chef
WORKDIR /app

# Install libclang and dependencies required by bindgen (rocksdb)
RUN apt-get update && apt-get install -y \
  clang \
  libclang-dev \
  llvm-dev \
  pkg-config \
  && rm -rf /var/lib/apt/lists/*

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

# Copy back the build dependencies including libclang
COPY --from=planner /app/recipe.json recipe.json
# crates/vendored/ethrex-crypto is a [patch] path dependency, not a workspace
# member (see its VENDORED.md) — cargo-chef's recipe only tracks workspace
# members, so `cook` can't materialize a dummy for it. Its real source must
# be present on disk before `cook` resolves the dependency graph.
COPY crates/vendored ./crates/vendored
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN cargo build --release -p helix-builder

FROM debian:stable-slim AS runtime
WORKDIR /app

RUN apt-get update && apt-get install -y \
  ca-certificates \
  && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/helix-builder ./

# 9876  merging TCP (relay connections)
# 8552  SSZ block validation (simulation role)
# 8551  authrpc / Engine API (beacon node)
# 8545  http json-rpc
# 30303 devp2p tcp+udp
# 9090  prometheus metrics
EXPOSE 9876 8552 8551 8545 30303/tcp 30303/udp 9090

ENTRYPOINT ["/app/helix-builder"]
