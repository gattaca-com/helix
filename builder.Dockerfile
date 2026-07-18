FROM lukemathwalker/cargo-chef:latest-rust-1.91 AS chef
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
# 8551  authrpc / Engine API (beacon node)
# 8545  http json-rpc
# 30303 devp2p tcp+udp
# 9090  prometheus metrics
EXPOSE 9876 8551 8545 30303/tcp 30303/udp 9090

ENTRYPOINT ["/app/helix-builder"]
