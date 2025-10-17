FROM lukemathwalker/cargo-chef:latest-rust-1.90 AS chef
WORKDIR /app

# Install libclang and dependencies required by bindgen
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
RUN cargo build --release --bin helix-simulator

FROM debian:stable-slim AS runtime
WORKDIR /app

RUN apt-get update && apt-get install -y \
  ca-certificates \
  && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/helix-simulator ./

ENTRYPOINT ["/app/helix-simulator"]
